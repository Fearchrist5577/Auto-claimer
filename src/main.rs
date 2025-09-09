use std::{fs, path::PathBuf, str::FromStr, sync::{Arc, mpsc::{self, Sender, Receiver}, atomic::{AtomicBool, Ordering}}};
use std::time::{Duration, Instant};

use dirs::home_dir;
use eframe::egui;
use ethers::prelude::*;
use hex::FromHex;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

const DEFAULT_RPC: &str = "https://rpc.linea.build";
const DEFAULT_CONTRACT: &str = "0x7ec77150b33910a9c33b7e3881b84b254060dfb5";

#[derive(Serialize, Deserialize, Clone)]
struct KeystoreFile {
    pub pk_hex: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct AppConfigFile {
    pub rpc: String,
    pub contract: String,
    pub fallback_rpcs: Vec<String>,
    pub dest_address: String,
    pub auto_forward: bool,
    pub gas_reserve_wei: String,
    pub token_address: String,
    pub min_delta_wei: String,
    pub auto_claim_interval_secs: String,
}

fn app_dir() -> PathBuf {
    let mut p = home_dir().expect("no home dir");
    p.push(".linea-autoclaim");
    fs::create_dir_all(&p).ok();
    p
}

fn keystore_path() -> PathBuf {
    let mut p = app_dir();
    p.push("keystore.json");
    p
}

fn config_path() -> PathBuf {
    let mut p = app_dir();
    p.push("config.json");
    p
}

fn pk_from_keystore(ks: &KeystoreFile) -> anyhow::Result<Vec<u8>> {
    Ok(Vec::from_hex(ks.pk_hex.trim_start_matches("0x"))?)
}

fn save_keystore(ks: &KeystoreFile) -> anyhow::Result<()> {
    let data = serde_json::to_vec_pretty(ks)?;
    fs::write(keystore_path(), data)?;
    Ok(())
}

fn load_keystore() -> anyhow::Result<KeystoreFile> {
    let data = fs::read(keystore_path())?;
    let ks: KeystoreFile = serde_json::from_slice(&data)?;
    Ok(ks)
}

fn save_config(cfg: &AppConfigFile) -> anyhow::Result<()> {
    let data = serde_json::to_vec_pretty(cfg)?;
    fs::write(config_path(), data)?;
    Ok(())
}

fn load_config() -> anyhow::Result<AppConfigFile> {
    let data = fs::read(config_path())?;
    let cfg: AppConfigFile = serde_json::from_slice(&data)?;
    Ok(cfg)
}

// Minimal ABI needed by the tool.
abigen!(IAirdrop, r#"[ 
    function claim()
    function calculateAllocation(address) view returns (uint256)
    function hasClaimed(address) view returns (bool)
]"#);

/// Sends claim() to the given airdrop after preflight checks.
async fn claim_airdrop(
    provider: &Provider<Http>,
    wallet: &LocalWallet,
    contract_addr: &str,
) -> anyhow::Result<String> {
    let to = Address::from_str(contract_addr)?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let signer = wallet.clone().with_chain_id(chain_id);
    let client = Arc::new(SignerMiddleware::new(provider.clone(), signer));
    let contract = IAirdrop::new(to, client.clone());

    let me = wallet.address();

    let alloc: U256 = contract
        .calculate_allocation(me)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("calculateAllocation() failed: {e}"))?;
    if alloc.is_zero() {
        anyhow::bail!("Allocation is zero — ensure ELIG is minted and airdrop funded.");
    }

    let already: bool = contract.has_claimed(me).call().await.unwrap_or(false);
    if already {
        anyhow::bail!(format!("Address {me:?} has already claimed."));
    }

    let tx = contract.claim();
    let pending = tx
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("claim() send failed: {e}"))?;

    if let Some(rcpt) = pending
        .await
        .map_err(|e| anyhow::anyhow!("claim() pending failed: {e}"))?
    {
        if rcpt.status == Some(U64::from(1u64)) {
            return Ok(format!(
                "Claim succeeded. tx: {:?}, block: {}",
                rcpt.transaction_hash,
                rcpt.block_number.unwrap_or_default()
            ));
        } else {
            anyhow::bail!("claim() reverted — check contract state & logs.");
        }
    } else {
        Ok("Submitted; provider returned no receipt yet.".to_string())
    }
}

async fn forward_eth(
    provider: &Provider<Http>,
    wallet: &LocalWallet,
    to_addr: &str,
    gas_reserve_wei: U256,
) -> anyhow::Result<String> {
    let to = Address::from_str(to_addr)?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let signer = wallet.clone().with_chain_id(chain_id);
    let client = Arc::new(SignerMiddleware::new(provider.clone(), signer));

    let me = wallet.address();
    let balance = client.get_balance(me, None).await?;
    if balance <= gas_reserve_wei {
        anyhow::bail!("Insufficient balance to forward after reserving gas");
    }
    let amount = balance - gas_reserve_wei;

    let tx = TransactionRequest::new().to(to).value(amount);
    let pending = client.send_transaction(tx, None).await?;
    if let Some(rcpt) = pending.await? {
        if rcpt.status == Some(U64::from(1u64)) {
            return Ok(format!("Forwarded {} wei to {:?}", amount, to));
        } else {
            anyhow::bail!("Forward tx reverted");
        }
    }
    Ok("Forward submitted; no receipt yet".to_string())
}

abigen!(IERC20, r#"[
    function balanceOf(address) view returns (uint256)
    function transfer(address to, uint256 value) returns (bool)
]"#);

async fn forward_erc20(
    provider: &Provider<Http>,
    wallet: &LocalWallet,
    token_addr: &str,
    dest_addr: &str,
) -> anyhow::Result<String> {
    let token = Address::from_str(token_addr)?;
    let dest = Address::from_str(dest_addr)?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let signer = wallet.clone().with_chain_id(chain_id);
    let client = Arc::new(SignerMiddleware::new(provider.clone(), signer));
    let erc20 = IERC20::new(token, client.clone());

    let me = wallet.address();
    let bal: U256 = erc20.balance_of(me).call().await?;
    if bal.is_zero() { anyhow::bail!("Token balance is zero; nothing to forward"); }

    let call = erc20.transfer(dest, bal);
    let pending = call.send().await?;
    if let Some(rcpt) = pending.await? {
        if rcpt.status == Some(U64::from(1u64)) {
            return Ok(format!("Forwarded {} tokens to {:?}", bal, dest));
        } else {
            anyhow::bail!("ERC20 transfer reverted");
        }
    }
    Ok("ERC20 transfer submitted; no receipt yet".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Home,
    Settings,
    Tokens,
}

struct GuiApp {
    rpc: String,
    contract: String,
    pk_hex: String,
    address: String,
    fallback_rpcs_text: String,
    dest_address: String,
    auto_forward: bool,
    gas_reserve_wei_input: String,
    token_address: String,
    status_lines: Vec<String>,
    runtime: tokio::runtime::Runtime,
    log_rx: Receiver<String>,
    log_tx: Sender<String>,
    is_busy: bool,
    // Auto-claim controls
    min_delta_wei_input: String,
    interval_secs_input: String,
    watcher_running: bool,
    watcher_cancel: Option<Arc<AtomicBool>>,
    // UI state
    current_tab: Tab,
    auto_scroll_logs: bool,
    show_logs_panel: bool,
    // Tokens tab state
    token_tab_selected: String,
    token_tab_running: bool,
    token_tab_log_rx: Receiver<String>,
    token_tab_log_tx: Sender<String>,
    token_tab_logs: Vec<String>,
    token_tab_auto_scroll: bool,
    token_tab_cancel: Option<Arc<AtomicBool>>,
    token_tab_interval_input: String,
    // Wallet balance state
    balance_text: String,
    balance_rx: Receiver<String>,
    balance_tx: Sender<String>,
    balance_inflight: bool,
    next_balance_check: Option<Instant>,
    // Network label state
    network_label: String,
    network_rx: Receiver<String>,
    network_tx: Sender<String>,
    last_rpc_seen: String,
    // UI: donate modal
    show_donate_modal: bool,
}

impl GuiApp {
    fn new() -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let (log_tx, log_rx) = mpsc::channel();
        let (token_tab_log_tx, token_tab_log_rx) = mpsc::channel();
        let (balance_tx, balance_rx) = mpsc::channel();
        let (network_tx, network_rx) = mpsc::channel();

        let mut rpc = DEFAULT_RPC.to_string();
        let mut contract = DEFAULT_CONTRACT.to_string();
        let mut fallback_rpcs_text = String::new();
        let mut dest_address = String::new();
        let mut auto_forward = false;
        let mut gas_reserve_wei_input = "200000000000000".to_string();
        let mut token_address = String::new();
        if let Ok(cfg) = load_config() {
            if !cfg.rpc.is_empty() { rpc = cfg.rpc; }
            if !cfg.contract.is_empty() { contract = cfg.contract; }
            if !cfg.fallback_rpcs.is_empty() { fallback_rpcs_text = cfg.fallback_rpcs.join("\n"); }
            if !cfg.dest_address.is_empty() { dest_address = cfg.dest_address; }
            if !cfg.gas_reserve_wei.is_empty() { gas_reserve_wei_input = cfg.gas_reserve_wei; }
            auto_forward = cfg.auto_forward;
            if !cfg.token_address.is_empty() { token_address = cfg.token_address; }
        }

        let mut pk_hex = String::new();
        let mut address = String::new();
        if let Ok(ks) = load_keystore() {
            pk_hex = ks.pk_hex;
            if let Ok(pk) = pk_from_keystore(&KeystoreFile { pk_hex: pk_hex.clone() }) {
                if let Ok(wallet) = LocalWallet::from_bytes(&pk) {
                    address = format!("{:?}", wallet.address());
                }
            }
        }

        Self {
            rpc,
            contract,
            pk_hex,
            address,
            fallback_rpcs_text,
            dest_address,
            auto_forward,
            gas_reserve_wei_input,
            token_address,
            status_lines: Vec::new(),
            runtime,
            log_rx,
            log_tx,
            is_busy: false,
            min_delta_wei_input: "1".to_string(),
            interval_secs_input: "1".to_string(),
            watcher_running: false,
            watcher_cancel: None,
            current_tab: Tab::Home,
            auto_scroll_logs: true,
            show_logs_panel: true,
            token_tab_selected: String::new(),
            token_tab_running: false,
            token_tab_log_rx,
            token_tab_log_tx,
            token_tab_logs: Vec::new(),
            token_tab_auto_scroll: true,
            token_tab_cancel: None,
            token_tab_interval_input: "1".to_string(),
            balance_text: String::new(),
            balance_rx,
            balance_tx,
            balance_inflight: false,
            next_balance_check: Some(Instant::now()),
            network_label: String::new(),
            network_rx,
            network_tx,
            last_rpc_seen: String::new(),
            show_donate_modal: false,
        }
    }

    fn log(&mut self, msg: impl Into<String>) {
        self.status_lines.push(msg.into());
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(line) = self.log_rx.try_recv() {
            self.status_lines.push(line);
        }
        while let Ok(b) = self.balance_rx.try_recv() {
            self.balance_text = b;
            self.balance_inflight = false;
        }
        while let Ok(n) = self.network_rx.try_recv() {
            self.network_label = n;
        }

        // Apply custom styling
        let mut visuals = egui::Visuals::dark();
        visuals.window_rounding = egui::Rounding::same(8.0);
        ctx.set_visuals(visuals);
        // Ensure periodic repaints for real-time logs
        ctx.request_repaint_after(std::time::Duration::from_millis(150));

        // If RPC changed, fetch immediately
        if self.last_rpc_seen != self.rpc {
            self.last_rpc_seen = self.rpc.clone();
            self.next_balance_check = Some(Instant::now());
        }

        // Periodic wallet balance + network refresh
        if !self.balance_inflight {
            let now = Instant::now();
            let should_fetch = self.next_balance_check.map(|t| now >= t).unwrap_or(false);
            if should_fetch {
                let rpc = self.rpc.clone();
                let fallbacks = self.fallback_rpcs_text.clone();
                let pk_hex = self.pk_hex.clone();
                let txb = self.balance_tx.clone();
                let txn = self.network_tx.clone();
                self.balance_inflight = true;
                self.next_balance_check = Some(now + Duration::from_secs(20));
                self.runtime.spawn(async move {
                    let provider = match GuiApp::build_provider_with_fallback(rpc, fallbacks, txb.clone()).await {
                        Some(p) => p,
                        None => return,
                    };
                    // Update network label
                    match provider.get_chainid().await {
                        Ok(cid) => {
                            let name = match cid.as_u64() {
                                1 => "Ethereum".to_string(),
                                10 => "Optimism".to_string(),
                                56 => "BNB Smart Chain".to_string(),
                                137 => "Polygon".to_string(),
                                8453 => "Base".to_string(),
                                59144 => "Linea".to_string(),
                                42161 => "Arbitrum One".to_string(),
                                43114 => "Avalanche C-Chain".to_string(),
                                other => format!("Chain {}", other),
                            };
                            let _ = txn.send(name);
                        }
                        Err(_) => { let _ = txn.send("(unknown)".to_string()); }
                    }
                    let pk_bytes: Vec<u8> = match Vec::from_hex(pk_hex.trim_start_matches("0x")) {
                        Ok(b) => b,
                        Err(_) => { let _ = txb.send("(no wallet)".to_string()); return; }
                    };
                    let wallet = match LocalWallet::from_bytes(&pk_bytes) { Ok(w) => w, Err(_) => { let _ = txb.send("(wallet error)".to_string()); return; } };
                    let addr = wallet.address();
                    match provider.get_balance(addr, None).await {
                        Ok(bal) => {
                            let eth = ethers::utils::format_units(bal, 18).unwrap_or_else(|_| bal.to_string());
                            let _ = txb.send(format!("{} ETH ({} wei)", eth, bal));
                        }
                        Err(e) => { let _ = txb.send(format!("balance error: {}", e)); }
                    }
                });
            }
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.heading("🚀 Auto-Claimer");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("💖 Donate").clicked() { self.show_donate_modal = true; }
                    ui.hyperlink_to("by MrCrypto", "https://x.com/Mr_CryptoYT");
                });
            });
            ui.add_space(8.0);
        });

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.selectable_value(&mut self.current_tab, Tab::Home, "Auto Claim");
                ui.selectable_value(&mut self.current_tab, Tab::Tokens, "Auto transfer");
                ui.selectable_value(&mut self.current_tab, Tab::Settings, "Settings");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.show_logs_panel, "Logs panel");
                });
            });
            ui.add_space(4.0);
        });

        // Right-side logs panel (toggleable)
        if self.show_logs_panel {
            egui::SidePanel::right("logs_panel")
                .resizable(true)
                .default_width(320.0)
                .min_width(260.0)
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.heading("📋 Activity Log");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Clear").clicked() { self.status_lines.clear(); }
                            ui.checkbox(&mut self.auto_scroll_logs, "Auto-scroll");
                        });
                    });
                    ui.separator();
                    ui.add_space(6.0);

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(self.auto_scroll_logs)
                        .show(ui, |ui| {
                            if self.status_lines.is_empty() {
                                ui.colored_label(egui::Color32::from_rgb(158, 158, 158), "No activity yet");
                            } else {
                                for line in &self.status_lines {
                                    ui.label(line);
                                }
                            }
                        });
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    match self.current_tab {
                        Tab::Home => self.show_home_tab(ui),
                        Tab::Tokens => self.show_tokens_tab(ui),
                        Tab::Settings => self.show_settings_tab(ui),
                    }
                });
        });

        if self.show_donate_modal {
            egui::Window::new("Support the project")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("If this app helped you, consider a donation:");
                    ui.add_space(8.0);
                    ui.monospace("ETH: 0x519e9aa581E8A00cf4aa51ffc85B5E2BD2BECA75");
                    ui.monospace("SOL: 5FW6WHGZFReH7XYHezhZijxPNtDVZjVLr3xffHrTFtzS");
                    ui.monospace("BTC: 33vsHnSafGMV6atqAqppDEBiFenCipQ4do");
                    ui.add_space(12.0);
                    if ui.button("Close").clicked() { self.show_donate_modal = false; }
                });
        }
    }
}

impl GuiApp {
    async fn build_provider_with_fallback(
        rpc: String,
        fallbacks_text: String,
        tx: Sender<String>,
    ) -> Option<Provider<Http>> {
        let mut urls: Vec<String> = Vec::new();
        urls.push(rpc);
        for line in fallbacks_text.lines() {
            let u = line.trim();
            if !u.is_empty() { urls.push(u.to_string()); }
        }

        for url in urls {
            match Provider::<Http>::try_from(url.clone()) {
                Ok(p) => {
                    let check = tokio::time::timeout(Duration::from_secs(3), p.get_chainid()).await;
                    match check {
                        Ok(Ok(_)) => { let _ = tx.send(format!("Using RPC: {}", url)); return Some(p); }
                        Ok(Err(e)) => { let _ = tx.send(format!("RPC failed {}: {}", url, e)); }
                        Err(_) => { let _ = tx.send(format!("RPC timeout: {}", url)); }
                    }
                }
                Err(e) => { let _ = tx.send(format!("Invalid RPC URL {}: {}", url, e)); }
            }
        }
        let _ = tx.send("No working RPC endpoint available".to_string());
        None
    }
    fn show_home_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        
        // Wallet status card
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(40, 44, 52))
            .rounding(8.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading("💳 Wallet Status");
                ui.separator();
                if self.address.is_empty() {
                    ui.colored_label(egui::Color32::from_rgb(255, 152, 0), "⚠️ No wallet configured");
                    ui.label("Please configure your wallet in Settings tab");
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Address:");
                        ui.strong(self.address.as_str());
                    });
                    ui.horizontal(|ui| {
                        ui.label("Network:");
                        if self.network_label.is_empty() { ui.label("Fetching…"); } else { ui.strong(self.network_label.as_str()); }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Balance:");
                        if self.balance_text.is_empty() { ui.label("Fetching…"); } else { ui.strong(self.balance_text.as_str()); }
                    });
                }
            });

        ui.add_space(16.0);

        // Removed Quick actions (Claim Now moved to Auto-claim section)
        ui.add_space(8.0);

        // Auto-claim section
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(40, 44, 52))
            .rounding(8.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading("Auto-claim");
                ui.separator();
                ui.add_space(8.0);
                ui.label("Automatically triggers claim when ETH deposit is detected");
                ui.add_space(12.0);
                
                // Auto-claim thresholds moved to Settings

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.heading("🔀 Auto-forward (ETH)");
                ui.add_space(6.0);
                ui.checkbox(&mut self.auto_forward, "Enable auto-forward after successful claim");
                ui.add_space(6.0);
                ui.label("Airdrop Contract Address:");
                ui.add_space(4.0);
                ui.text_edit_singleline(&mut self.contract);
                ui.add_space(6.0);
                ui.label("Claimed token address (ERC20, optional - forwards token if set):");
                ui.add_space(4.0);
                ui.text_edit_singleline(&mut self.token_address);
                ui.add_space(6.0);
                ui.label("Destination address (0x…):");
                ui.add_space(4.0);
                ui.text_edit_singleline(&mut self.dest_address);
                ui.add_space(6.0);
                ui.label("Gas reserve (wei) to keep for fees:");
                ui.add_space(4.0);
                ui.text_edit_singleline(&mut self.gas_reserve_wei_input);
                ui.add_space(8.0);
                if ui.button("💾 Save Auto-forward Settings").clicked() {
                    let mut cfg = load_config().unwrap_or_default();
                    cfg.auto_forward = self.auto_forward;
                    cfg.dest_address = self.dest_address.clone();
                    cfg.gas_reserve_wei = self.gas_reserve_wei_input.clone();
                    cfg.token_address = self.token_address.clone();
                    cfg.rpc = self.rpc.clone();
                    cfg.contract = self.contract.clone();
                    cfg.fallback_rpcs = self
                        .fallback_rpcs_text
                        .lines()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if let Err(e) = save_config(&cfg) { self.log(format!("❌ Save config failed: {e}")); }
                    else { self.log(format!("✅ Auto-forward settings saved to {}", config_path().display())); }
                }
                
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let running = self.watcher_running;
                    ui.add_enabled_ui(!running && !self.address.is_empty(), |ui| {
                        let start_btn = egui::Button::new(
                                egui::RichText::new("Start Auto-claim").color(egui::Color32::BLACK)
                            )
                            .fill(egui::Color32::from_rgb(76, 175, 80));
                        if ui.add(start_btn).clicked() {
                            let min_delta = match U256::from_dec_str(self.min_delta_wei_input.trim()) {
                                Ok(v) => v,
                                Err(_) => { self.log("❌ Invalid min delta (wei). Use decimal number."); return; }
                            };
                            let interval_secs: u64 = match self.interval_secs_input.trim().parse() {
                                Ok(v) if v > 0 => v,
                                _ => { self.log("❌ Invalid interval seconds. Use positive integer."); return; }
                            };
                            if self.pk_hex.trim().is_empty() { self.log("❌ Set a private key first."); return; }

                            let cancel = Arc::new(AtomicBool::new(false));
                            self.watcher_cancel = Some(cancel.clone());
                            self.watcher_running = true;

                            let rpc = self.rpc.clone();
                            let contract = self.contract.clone();
                            let pk_hex = self.pk_hex.clone();
                            let tx = self.log_tx.clone();
                            let fallbacks = self.fallback_rpcs_text.clone();
                            let auto_forward = self.auto_forward;
                            let dest_address = self.dest_address.clone();
                            let gas_reserve_wei_str = self.gas_reserve_wei_input.clone();
                            let token_address = self.token_address.clone();

                            self.runtime.spawn(async move {
                                let _ = tx.send(" Auto-claim watcher started.".to_string());
                                let provider = match GuiApp::build_provider_with_fallback(rpc.clone(), fallbacks.clone(), tx.clone()).await {
                                    Some(p) => p,
                                    None => return,
                                };
                                let pk_bytes: Vec<u8> = match Vec::from_hex(pk_hex.trim_start_matches("0x")) {
                                    Ok(b) => b,
                                    Err(e) => { let _ = tx.send(format!("❌ Invalid private key hex: {e}")); return; }
                                };
                                let wallet = match LocalWallet::from_bytes(&pk_bytes) {
                                    Ok(w) => w,
                                    Err(e) => { let _ = tx.send(format!("❌ Wallet error: {e}")); return; }
                                };
                                let me = wallet.address();
                                let mut last_balance: U256 = match provider.get_balance(me, None).await {
                                    Ok(b) => b,
                                    Err(e) => { let _ = tx.send(format!("❌ get_balance failed: {e}")); return; }
                                };
                                let _ = tx.send(format!("📊 Initial balance: {} wei", last_balance));

                                loop {
                                    if cancel.load(Ordering::Relaxed) { let _ = tx.send("🔴 Watcher stopped.".to_string()); break; }
                                    tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                                    if cancel.load(Ordering::Relaxed) { let _ = tx.send("🔴 Watcher stopped.".to_string()); break; }
                                    let bal = match provider.get_balance(me, None).await {
                                        Ok(b) => b,
                                        Err(e) => { let _ = tx.send(format!("❌ get_balance failed: {e}")); continue; }
                                    };
                                    if bal > last_balance {
                                        let delta = bal - last_balance;
                                        let _ = tx.send(format!("💰 Deposit detected: {} wei", delta));
                                        if delta >= min_delta {
                                            let _ = tx.send("🎯 Attempting claim()…".to_string());
                                            match claim_airdrop(&provider, &wallet, &contract).await {
                                                Ok(msg) => {
                                                    let _ = tx.send(format!("✅ {msg}"));
                                                    if auto_forward {
                                                        if dest_address.is_empty() { let _ = tx.send("⚠️ Auto-forward enabled but destination is empty".to_string()); }
                                                        else {
                                                            if !token_address.trim().is_empty() {
                                                                let _ = tx.send("↪️ Forwarding claimed token to destination…".to_string());
                                                                match forward_erc20(&provider, &wallet, &token_address, &dest_address).await {
                                                                    Ok(m) => { let _ = tx.send(format!("✅ {m}")); }
                                                                    Err(e) => { let _ = tx.send(format!("❌ Token forward failed: {e}")); }
                                                                }
                                                            } else {
                                                                let gas_reserve = U256::from_dec_str(gas_reserve_wei_str.trim()).unwrap_or(U256::from(200000000000000u64));
                                                                let _ = tx.send("↪️ Forwarding claimed ETH to destination…".to_string());
                                                                match forward_eth(&provider, &wallet, &dest_address, gas_reserve).await {
                                                                    Ok(m) => { let _ = tx.send(format!("✅ {m}")); }
                                                                    Err(e) => { let _ = tx.send(format!("❌ ETH forward failed: {e}")); }
                                                                }
                                                            }
                                                        }
                                                    }
                                                },
                                                Err(e) => { let _ = tx.send(format!("❌ Claim failed: {e}")); },
                                            }
                                        }
                                        last_balance = bal;
                                    } else if bal < last_balance {
                                        // Balance decreased (spent); update baseline
                                        last_balance = bal;
                                    }
                                }
                            });
                        }
                    });

                    ui.add_enabled_ui(running, |ui| {
                        let stop_btn = egui::Button::new(
                                egui::RichText::new("Stop Auto-claim").color(egui::Color32::BLACK)
                            )
                            .fill(egui::Color32::from_rgb(244, 67, 54));
                        if ui.add(stop_btn).clicked() {
                            if let Some(c) = &self.watcher_cancel { c.store(true, Ordering::Relaxed); }
                            self.watcher_running = false;
                        }
                    });

                    // Claim Now next to Stop button (same size, purple color)
                    let claim_btn = egui::Button::new(
                            egui::RichText::new("Claim Now").color(egui::Color32::BLACK)
                        )
                        .fill(egui::Color32::from_rgb(76, 175, 80));
                    ui.add_enabled_ui(!self.is_busy && !self.address.is_empty(), |ui| {
                        if ui.add(claim_btn).clicked() {
                            let rpc = self.rpc.clone();
                            let contract = self.contract.clone();
                            let pk_hex = self.pk_hex.clone();
                            let tx = self.log_tx.clone();
                            let fallbacks = self.fallback_rpcs_text.clone();
                            let auto_forward = self.auto_forward;
                            let dest_address = self.dest_address.clone();
                            let gas_reserve_wei_str = self.gas_reserve_wei_input.clone();
                            let token_address = self.token_address.clone();
                            self.is_busy = true;
                            self.runtime.spawn(async move {
                                let _ = tx.send("🚀 Starting claim…".to_string());
                                let provider = match GuiApp::build_provider_with_fallback(rpc.clone(), fallbacks.clone(), tx.clone()).await {
                                    Some(p) => p,
                                    None => return,
                                };
                                let pk_bytes: Vec<u8> = match Vec::from_hex(pk_hex.trim_start_matches("0x")) {
                                    Ok(b) => b,
                                    Err(e) => { let _ = tx.send(format!("❌ Invalid private key hex: {e}")); return; }
                                };
                                let wallet = match LocalWallet::from_bytes(&pk_bytes) {
                                    Ok(w) => w,
                                    Err(e) => { let _ = tx.send(format!("❌ Wallet error: {e}")); return; }
                                };
                                match claim_airdrop(&provider, &wallet, &contract).await {
                                    Ok(msg) => {
                                        let _ = tx.send(format!("✅ {msg}"));
                                        if auto_forward {
                                            if dest_address.is_empty() { let _ = tx.send("⚠️ Auto-forward enabled but destination is empty".to_string()); }
                                            else {
                                                if !token_address.trim().is_empty() {
                                                    let _ = tx.send("↪️ Forwarding claimed token to destination…".to_string());
                                                    match forward_erc20(&provider, &wallet, &token_address, &dest_address).await {
                                                        Ok(m) => { let _ = tx.send(format!("✅ {m}")); }
                                                        Err(e) => { let _ = tx.send(format!("❌ Token forward failed: {e}")); }
                                                    }
                                                } else {
                                                    let gas_reserve = U256::from_dec_str(gas_reserve_wei_str.trim()).unwrap_or(U256::from(200000000000000u64));
                                                    let _ = tx.send("↪️ Forwarding claimed ETH to destination…".to_string());
                                                    match forward_eth(&provider, &wallet, &dest_address, gas_reserve).await {
                                                        Ok(m) => { let _ = tx.send(format!("✅ {m}")); }
                                                        Err(e) => { let _ = tx.send(format!("❌ ETH forward failed: {e}")); }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => { let _ = tx.send(format!("❌ Claim failed: {e}")); }
                                }
                                let _ = tx.send("✨ Done.".to_string());
                            });
                        }
                    });
                });
                
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if self.watcher_running {
                        ui.colored_label(egui::Color32::from_rgb(76, 175, 80), "● Running");
                    } else {
                        ui.colored_label(egui::Color32::from_rgb(158, 158, 158), "● Stopped");
                    }
                });
            });

        // Logs moved to right panel
    }

    fn show_settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        
        // Connection settings
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(40, 44, 52))
            .rounding(8.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading("🌐 Connection Settings");
                ui.separator();
                ui.add_space(12.0);
                
                ui.label("RPC Endpoint:");
                ui.add_space(4.0);
                ui.text_edit_singleline(&mut self.rpc);
                
                ui.add_space(12.0);
                ui.label("Fallback RPCs (one per line):");
                ui.add_space(4.0);
                egui::TextEdit::multiline(&mut self.fallback_rpcs_text)
                    .hint_text("https://linea-mainnet.g.alchemy.com/v2/KEY\nhttps://mainnet.infura.io/v3/KEY")
                    .desired_rows(4)
                    .show(ui);

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Get API keys:");
                    ui.hyperlink_to("Alchemy (dashboard)", "https://dashboard.alchemy.com/");
                    ui.hyperlink_to("Infura (dashboard)", "https://app.infura.io/");
                });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.heading("Auto-claim Thresholds");
                ui.add_space(6.0);
                egui::Grid::new("auto_claim_thresholds")
                    .num_columns(2)
                    .spacing([40.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Min deposit (wei):");
                        ui.text_edit_singleline(&mut self.min_delta_wei_input);
                        ui.end_row();

                        ui.label("Check interval (s):");
                        ui.text_edit_singleline(&mut self.interval_secs_input);
                        ui.end_row();
                    });

                ui.add_space(16.0);
                if ui.button("💾 Save Connection Settings").clicked() {
                    let fallbacks: Vec<String> = self
                        .fallback_rpcs_text
                        .lines()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let mut cfg = load_config().unwrap_or_default();
                    cfg.rpc = self.rpc.clone();
                    cfg.contract = self.contract.clone();
                    cfg.fallback_rpcs = fallbacks;
                    // preserve/merge auto-forward fields from UI
                    cfg.auto_forward = self.auto_forward;
                    cfg.dest_address = self.dest_address.clone();
                    cfg.gas_reserve_wei = self.gas_reserve_wei_input.clone();
                    cfg.min_delta_wei = self.min_delta_wei_input.clone();
                    cfg.auto_claim_interval_secs = self.interval_secs_input.clone();
                    let cfg = cfg;
                    if let Err(e) = save_config(&cfg) { 
                        self.log(format!("❌ Save config failed: {e}")); 
                    } else { 
                        self.log(format!("✅ Config saved to {}", config_path().display())); 
                    }
                }
            });
        
        ui.add_space(16.0);
        
        // Wallet settings
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(40, 44, 52))
            .rounding(8.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading("🔐 Wallet Settings");
                ui.separator();
                ui.add_space(12.0);
                
                ui.label("Private Key (hex format):");
                ui.add_space(4.0);
                ui.text_edit_singleline(&mut self.pk_hex);
                ui.add_space(4.0);
                ui.label("Enter your private key starting with 0x...");
                
                ui.add_space(16.0);
                if ui.button("🔑 Import Wallet").clicked() {
                    match Vec::from_hex(self.pk_hex.trim_start_matches("0x")) {
                        Ok(mut bytes) => {
                            if bytes.len() != 32 {
                                self.log("❌ Private key must be 32 bytes hex.");
                            } else {
                                let ks = KeystoreFile { pk_hex: format!("0x{}", hex::encode(&bytes)) };
                                bytes.zeroize();
                                if let Err(e) = save_keystore(&ks) { 
                                    self.log(format!("❌ Save keystore failed: {e}")); 
                                } else {
                                    self.log(format!("✅ Keystore saved to {}", keystore_path().display()));
                                    if let Ok(pk) = pk_from_keystore(&ks) {
                                        if let Ok(wallet) = LocalWallet::from_bytes(&pk) {
                                            self.address = format!("{:?}", wallet.address());
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => self.log(format!("❌ Invalid hex: {e}")),
                    }
                }
                
                if !self.address.is_empty() {
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Current address:");
                        ui.strong(self.address.as_str());
                    });
                }
            });
        
        // (Auto-forward moved to Auto Claim tab)
        
        // Info section
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(40, 44, 52))
            .rounding(8.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading("ℹ️ Information");
                ui.separator();
                ui.add_space(8.0);
                
                ui.label("Configuration files are stored in:");
                ui.monospace(app_dir().display().to_string());
                ui.add_space(8.0);
                ui.label("• keystore.json - Wallet private key (unencrypted)");
                ui.label("• config.json - RPC and contract settings");
            });
    }

    fn show_tokens_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(40, 44, 52))
            .rounding(8.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading("🪙 Token Auto-forward");
                ui.separator();
                ui.add_space(8.0);

                ui.label("Select ERC20 token contract to monitor (0x…):");
                ui.add_space(4.0);
                ui.text_edit_singleline(&mut self.token_tab_selected);

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Interval (s):");
                    ui.text_edit_singleline(&mut self.token_tab_interval_input);
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!self.token_tab_running, |ui| {
                        if ui.button("▶️ Start").clicked() {
                            let rpc = self.rpc.clone();
                            let fallbacks = self.fallback_rpcs_text.clone();
                            let pk_hex = self.pk_hex.clone();
                            let dest_address = self.dest_address.clone();
                            let token_addr = self.token_tab_selected.clone();
                            let interval_secs: u64 = self.token_tab_interval_input.trim().parse().unwrap_or(6);
                            let tx = self.token_tab_log_tx.clone();
                            let cancel = Arc::new(AtomicBool::new(false));
                            self.token_tab_cancel = Some(cancel.clone());
                            if dest_address.trim().is_empty() { let _ = tx.send("Destination address is empty (Settings)".to_string()); return; }
                            if token_addr.trim().is_empty() { let _ = tx.send("Token address is empty".to_string()); return; }
                            self.token_tab_running = true;
                            self.runtime.spawn(async move {
                                let _ = tx.send("Token watcher started".to_string());
                                let provider = match GuiApp::build_provider_with_fallback(rpc.clone(), fallbacks.clone(), tx.clone()).await {
                                    Some(p) => p,
                                    None => return,
                                };
                                let pk_bytes: Vec<u8> = match Vec::from_hex(pk_hex.trim_start_matches("0x")) {
                                    Ok(b) => b,
                                    Err(e) => { let _ = tx.send(format!("Invalid private key hex: {e}")); return; }
                                };
                                let wallet = match LocalWallet::from_bytes(&pk_bytes) {
                                    Ok(w) => w,
                                    Err(e) => { let _ = tx.send(format!("Wallet error: {e}")); return; }
                                };
                                let token_addr_parsed = match Address::from_str(&token_addr) {
                                    Ok(a) => a,
                                    Err(e) => { let _ = tx.send(format!("Invalid token address: {e}")); return; }
                                };
                                loop {
                                    // poll every 6s
                                    tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                                    if cancel.load(Ordering::Relaxed) { let _ = tx.send("Token watcher stopped".to_string()); break; }
                                    // check token balance then forward with detailed logs
                                    let view = IERC20::new(token_addr_parsed, Arc::new(provider.clone()));
                                    match view.balance_of(wallet.address()).call().await {
                                        Ok(bal) => {
                                            if bal > U256::zero() {
                                                let _ = tx.send(format!("🔎 Detected token balance: {}", bal));
                                                let _ = tx.send("➡️ Processing forwarding…".to_string());
                                                match forward_erc20(&provider, &wallet, &token_addr, &dest_address).await {
                                                    Ok(m) => { let _ = tx.send(format!("✅ {m}")); let _ = tx.send("✅ Forward complete".to_string()); }
                                                    Err(e) => { let _ = tx.send(format!("❌ Token forward failed: {e}")); }
                                                }
                                            } else {
                                                let _ = tx.send("⏳ No token balance; waiting…".to_string());
                                            }
                                        }
                                        Err(e) => { let _ = tx.send(format!("ℹ️ balanceOf failed: {e}")); }
                                    }
                                }
                            });
                        }
                    });
                    ui.add_enabled_ui(self.token_tab_running, |ui| {
                        if ui.button("⏹️ Stop").clicked() {
                            if let Some(c) = &self.token_tab_cancel { c.store(true, Ordering::Relaxed); }
                            self.token_tab_running = false;
                        }
                    });
                });
            });

        ui.add_space(12.0);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(30, 33, 39))
            .rounding(8.0)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading("📋 Token Log");
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Clear").clicked() { self.token_tab_logs.clear(); }
                    ui.checkbox(&mut self.token_tab_auto_scroll, "Auto-scroll");
                });
                ui.add_space(6.0);
                while let Ok(line) = self.token_tab_log_rx.try_recv() {
                    self.token_tab_logs.push(line);
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.token_tab_auto_scroll)
                    .max_height(260.0)
                    .show(ui, |ui| {
                        if self.token_tab_logs.is_empty() {
                            ui.colored_label(egui::Color32::from_rgb(158, 158, 158), "No activity yet");
                        } else {
                            for line in &self.token_tab_logs {
                                ui.label(line);
                            }
                        }
                    });
            });
    }
}

fn main() -> eframe::Result<()> {
    dotenvy::dotenv().ok();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(1000.0, 850.0))
            .with_min_inner_size(egui::vec2(1100.0, 800.0)),
        ..Default::default()
    };
    eframe::run_native("Auto-Claim", native_options, Box::new(|_cc| Box::new(GuiApp::new())))
}
