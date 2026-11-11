//! Telegram update handling. Only private chats can trade. Every button is a
//! server-side, user+chat-bound, expiring token; spending buttons are
//! single-use and map to a deterministic idempotency key, so replays,
//! double taps and webhook retries can never create a second order.

use super::render::{self, esc, short};
use crate::appstore::{CallbackAction, CallbackCheck, CallbackToken, OutboxMessage, User};
use crate::auth::new_callback_token;
use crate::service::{self, ServiceError};
use crate::state::AppState;
use serde_json::{json, Value};
use trader_domain::{AmountSpec, Bps, Commitment, Lamports, Network, Side};
use uuid::Uuid;

const TRADE_BUTTON_TTL: i64 = 120;
const NAV_BUTTON_TTL: i64 = 600;

pub async fn reply(st: &AppState, chat_id: i64, html: String, buttons: Value) {
    let m = OutboxMessage {
        id: Uuid::new_v4(),
        chat_id,
        text: html,
        buttons,
        dedupe_key: format!("reply:{}", Uuid::new_v4()),
        attempts: 0,
        next_attempt_at: chrono::Utc::now(),
        edit_message_id: None,
    };
    if st.app.enqueue(&m).await.is_ok() {
        st.outbox_notify.notify_one();
    }
}

pub async fn button(st: &AppState, user: &User, chat_id: i64, text: &str, action: CallbackAction, ttl: i64) -> Value {
    let token = new_callback_token();
    let t = CallbackToken { token: token.clone(), user_id: user.id, telegram_id: user.telegram_id, chat_id, action, expires_at: chrono::Utc::now() + chrono::Duration::seconds(ttl), used_at: None };
    let _ = st.app.create_callback(&t).await;
    json!({"text": text, "callback_data": format!("t:{token}")})
}

fn webapp_button(st: &AppState, path: &str) -> Option<Value> {
    let base = st.cfg.public_url.as_ref().filter(|u| u.starts_with("https://"))?;
    Some(json!({"text": "📱 Open Mini App", "web_app": {"url": format!("{}{}", base.trim_end_matches('/'), path)}}))
}

pub async fn handle_update(st: &AppState, upd: Value) {
    if let Some(cb) = upd.get("callback_query") {
        handle_callback(st, cb).await;
    } else if let Some(msg) = upd.get("message") {
        handle_message(st, msg).await;
    }
}

fn looks_like_mint(s: &str) -> bool {
    (32..=44).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() && !"0OIl".contains(c))
}

async fn handle_message(st: &AppState, msg: &Value) {
    let chat_id = msg.pointer("/chat/id").and_then(|v| v.as_i64()).unwrap_or(0);
    let chat_type = msg.pointer("/chat/type").and_then(|v| v.as_str()).unwrap_or("");
    let Some(from_id) = msg.pointer("/from/id").and_then(|v| v.as_i64()) else { return };
    if msg.pointer("/from/is_bot").and_then(|v| v.as_bool()) == Some(true) {
        return;
    }
    if chat_type != "private" {
        // Never trade from groups (anyone in the group could press buttons).
        return;
    }
    if !st.allow(&format!("tg:{from_id}"), 40) {
        return;
    }
    let username = msg.pointer("/from/username").and_then(|v| v.as_str()).map(str::to_string);
    let text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let user = match service::ensure_tg_user(st, from_id, username).await {
        Ok(u) => u,
        Err(e) => return reply(st, chat_id, format!("Internal error: {}", esc(&e.to_string())), json!(null)).await,
    };
    if text.is_empty() {
        return;
    }
    let (cmd, args) = match text.strip_prefix('/') {
        Some(rest) => {
            let mut it = rest.split_whitespace();
            let c = it.next().unwrap_or("").split('@').next().unwrap_or("").to_ascii_lowercase();
            (c, it.map(str::to_string).collect::<Vec<_>>())
        }
        None => (String::new(), vec![]),
    };
    let r = match cmd.as_str() {
        "start" => cmd_start(st, &user, chat_id, args.first().cloned()).await,
        "help" => Ok(reply(st, chat_id, render::HELP.into(), json!(null)).await),
        "wallet" | "balance" => cmd_wallet(st, &user, chat_id).await,
        "buy" => cmd_trade(st, &user, chat_id, Side::Buy, &args).await,
        "sell" => cmd_trade(st, &user, chat_id, Side::Sell, &args).await,
        "positions" => cmd_positions(st, &user, chat_id).await,
        "orders" => cmd_orders(st, &user, chat_id).await,
        "settings" => cmd_settings(st, &user, chat_id).await,
        "set" => cmd_set(st, &user, chat_id, &args).await,
        "track" => cmd_track(st, &user, chat_id, &args, true).await,
        "untrack" => cmd_track(st, &user, chat_id, &args, false).await,
        "pause" | "resume" => cmd_pause(st, &user, chat_id, cmd == "pause").await,
        "status" => Ok(reply(st, chat_id, format!("{}<pre>{}</pre>", render::mode_banner(st.cfg.mode), esc(&serde_json::to_string_pretty(&service::provider_health(st)).unwrap_or_default())), json!(null)).await),
        "copy" => Ok(reply(st, chat_id, "Copy trading is <b>not available</b> in this build (P1, disabled by default). Use /track for a public-wallet watchlist.".into(), json!(null)).await),
        "referrals" => Ok(reply(st, chat_id, "Referrals are <b>not implemented</b> (P2). No rewards program is running.".into(), json!(null)).await),
        "export" => Ok(reply(st, chat_id, "🔒 Key export is <b>disabled</b> in this build. Private keys and seed phrases are never sent through Telegram. Withdraw by trading back to SOL; withdrawals to external addresses are not yet available.".into(), json!(null)).await),
        "support" => Ok(reply(st, chat_id, "Support: this is a private beta. Contact the operator who invited you. We will never DM you first or ask for keys.".into(), json!(null)).await),
        "" if looks_like_mint(&text) => token_panel(st, &user, chat_id, &text).await,
        _ => Ok(reply(st, chat_id, "Unknown command. Send /help, or paste a token mint address.".into(), json!(null)).await),
    };
    if let Err(e) = r {
        reply(st, chat_id, format!("❌ {}", esc(&e.to_string())), json!(null)).await;
    }
}

async fn cmd_start(st: &AppState, user: &User, chat_id: i64, payload: Option<String>) -> Result<(), ServiceError> {
    if !user.beta_allowed {
        reply(st, chat_id, "This bot is in a closed private beta. Your Telegram account is not on the tester list.".into(), json!(null)).await;
        return Ok(());
    }
    let wallet = service::default_wallet(st, user).await?;
    let mut rows = vec![];
    if wallet.is_none() {
        rows.push(vec![button(st, user, chat_id, "🔑 Create test trading wallet", CallbackAction::CreateWallet, NAV_BUTTON_TTL).await]);
    }
    if let Some(b) = webapp_button(st, "/") {
        rows.push(vec![b]);
    }
    let mut text = format!("{}{}", render::mode_banner(st.cfg.mode), render::INTRO);
    if let Some(w) = &wallet {
        text.push_str(&format!("\n\nYour wallet: <code>{}</code>", esc(&w.pubkey)));
    }
    text.push_str("\n\nExternal-wallet signing: not yet available in the bot (planned for the Mini App).\nSend /help for commands.");
    reply(st, chat_id, text, json!({"inline_keyboard": rows})).await;
    if let Some(p) = payload.filter(|p| looks_like_mint(p)) {
        token_panel(st, user, chat_id, &p).await?;
    }
    Ok(())
}

async fn cmd_wallet(st: &AppState, user: &User, chat_id: i64) -> Result<(), ServiceError> {
    let Some(w) = service::default_wallet(st, user).await? else {
        let b = button(st, user, chat_id, "🔑 Create test trading wallet", CallbackAction::CreateWallet, NAV_BUTTON_TTL).await;
        reply(st, chat_id, "No wallet yet.".into(), json!({"inline_keyboard": [[b]]})).await;
        return Ok(());
    };
    let pk = trader_domain::pubkey::parse(&w.pubkey).map_err(ServiceError::Internal)?;
    let bal = st.rpc.get_balance(&pk, Commitment::Confirmed).await.map(|(l, _)| format!("{} SOL", render::sol(l))).unwrap_or_else(|_| "unavailable".into());
    let mut rows = vec![];
    if matches!(st.cfg.mode, Network::Mock | Network::Localnet | Network::Devnet) {
        rows.push(vec![button(st, user, chat_id, "💧 Airdrop test SOL", CallbackAction::MockAirdrop, NAV_BUTTON_TTL).await]);
    }
    let text = format!(
        "{}<b>Wallet</b> ({}): <code>{}</code>\nBalance: {}\nSigner: managed by this service (envelope-encrypted at rest; decrypted in server memory to sign). Not hardware-isolated.\n⚠️ Fund only small amounts. Export is disabled.",
        render::mode_banner(st.cfg.mode),
        esc(w.network.as_str()),
        esc(&w.pubkey),
        esc(&bal)
    );
    reply(st, chat_id, text, json!({"inline_keyboard": rows})).await;
    Ok(())
}

async fn cmd_trade(st: &AppState, user: &User, chat_id: i64, side: Side, args: &[String]) -> Result<(), ServiceError> {
    let (Some(mint), Some(amt)) = (args.first(), args.get(1)) else {
        return Err(ServiceError::BadRequest(if side == Side::Buy { "usage: /buy <mint> <SOL amount>".into() } else { "usage: /sell <mint> <percent 1-100>".into() }));
    };
    let mint = service::parse_mint_arg(mint)?;
    let amount = match side {
        Side::Buy => AmountSpec::ExactSolIn { lamports: Lamports::from_sol_str(amt).map_err(|e| ServiceError::BadRequest(e.to_string()))? },
        Side::Sell => {
            let pct: u32 = amt.trim_end_matches('%').parse().map_err(|_| ServiceError::BadRequest("percent must be 1-100".into()))?;
            if !(1..=100).contains(&pct) {
                return Err(ServiceError::BadRequest("percent must be 1-100".into()));
            }
            AmountSpec::SellPercent { bps: Bps::new(pct * 100).unwrap() }
        }
    };
    let w = service::default_wallet(st, user).await?.ok_or(ServiceError::NoWallet)?;
    // Commands are not idempotent across separate messages (each /buy is a new
    // intent), but webhook retries of the SAME update are deduped by update_id.
    let intent = service::build_intent(st, user, &w, side, mint, amount, format!("tg:cmd:{}", Uuid::new_v4()), None).await?;
    let o = service::submit(st, user, intent, None).await?;
    reply(st, chat_id, format!("⏳ Order <code>{}</code> created — preparing quote…", o.id), json!(null)).await;
    Ok(())
}

async fn token_panel(st: &AppState, user: &User, chat_id: i64, mint_s: &str) -> Result<(), ServiceError> {
    let mint = service::parse_mint_arg(mint_s)?;
    let info = service::token_info(st, mint).await;
    let mut text = format!("{}<b>Token</b> <code>{}</code>\n", render::mode_banner(st.cfg.mode), esc(&info.mint));
    match (&info.adapter, &info.market) {
        (Some(a), Some(m)) => {
            let liq = m.get("liquidity_sol_lamports").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()).map(|l| format!("{} SOL", render::sol(l))).unwrap_or_else(|| "unknown".into());
            text.push_str(&format!("Market: {} · Liquidity: {} · Source: {}\n", esc(a), esc(&liq), esc(m.get("source").and_then(|v| v.as_str()).unwrap_or("?"))));
        }
        _ => text.push_str(&format!("Market: <b>not supported</b> — {}\n", esc(info.unsupported_reason.as_deref().unwrap_or("unknown")))),
    }
    match &info.risk {
        Some(r) => {
            for f in &r.flags {
                let icon = match f.level {
                    trader_execution::RiskLevel::Block => "⛔",
                    trader_execution::RiskLevel::Warn => "⚠️",
                    trader_execution::RiskLevel::Info => "ℹ️",
                };
                text.push_str(&format!("{icon} {}\n", esc(&f.message)));
            }
            text.push_str(&format!("Top-10 holders: {}\n", r.top10_holder_bps.map(|b| format!("{}% (includes curve/pool account)", b / 100)).unwrap_or_else(|| "Unknown".into())));
        }
        None => text.push_str("Mint data: Unknown (not available from RPC)\n"),
    }
    text.push_str(&format!("<i>{}</i>", esc(info.disclaimer)));
    let mut rows: Vec<Vec<Value>> = vec![];
    if info.supported && user.beta_allowed {
        let s = service::settings(st, user).await?;
        let mut buys = vec![];
        for l in &s.buy_presets {
            buys.push(button(st, user, chat_id, &format!("Buy {}", render::sol(l.0)), CallbackAction::Buy { mint: info.mint.clone(), lamports: l.0 }, TRADE_BUTTON_TTL).await);
        }
        for chunk in buys.chunks(3) {
            rows.push(chunk.to_vec());
        }
        let mut sells = vec![];
        for b in &s.sell_presets_bps {
            sells.push(button(st, user, chat_id, &format!("Sell {}%", b / 100), CallbackAction::Sell { mint: info.mint.clone(), bps: *b }, TRADE_BUTTON_TTL).await);
        }
        rows.push(sells);
        rows.push(vec![button(st, user, chat_id, "🔄 Refresh", CallbackAction::Refresh { mint: info.mint.clone() }, NAV_BUTTON_TTL).await]);
    }
    reply(st, chat_id, text, json!({"inline_keyboard": rows})).await;
    Ok(())
}

async fn cmd_positions(st: &AppState, user: &User, chat_id: i64) -> Result<(), ServiceError> {
    let p = service::portfolio(st, user).await?;
    let mut text = format!("{}<b>Positions</b> (cost basis: weighted average, fees included)\n", render::mode_banner(st.cfg.mode));
    let mut any = false;
    for w in p["wallets"].as_array().cloned().unwrap_or_default() {
        for pos in w["positions"].as_array().cloned().unwrap_or_default() {
            any = true;
            let lam = |k: &str| pos[k].as_str().and_then(|s| s.parse::<i128>().ok());
            let fmt = |v: Option<i128>| v.map(|x| format!("{}{} SOL", if x < 0 { "-" } else { "" }, render::sol(x.unsigned_abs() as u64))).unwrap_or_else(|| "unavailable".into());
            text.push_str(&format!(
                "\n<code>{}</code> qty {} · cost {} · realized {} · unrealized {} ({})",
                esc(&short(pos["mint"].as_str().unwrap_or(""))),
                esc(pos["qty_raw"].as_str().unwrap_or("0")),
                fmt(lam("cost_basis_lamports")),
                fmt(lam("realized_pnl_lamports")),
                fmt(lam("unrealized_pnl_lamports")),
                esc(pos["price_source"].as_str().unwrap_or(""))
            ));
        }
    }
    if !any {
        text.push_str("\nNo positions yet.");
    }
    reply(st, chat_id, text, json!(null)).await;
    Ok(())
}

async fn cmd_orders(st: &AppState, user: &User, chat_id: i64) -> Result<(), ServiceError> {
    let orders = st.orders.list_orders(user.id, 10).await.map_err(|e| ServiceError::Internal(e.to_string()))?;
    let mut text = "<b>Recent orders</b>".to_string();
    if orders.is_empty() {
        text.push_str("\nNone yet.");
    }
    for o in orders {
        text.push_str(&format!(
            "\n{} {} <code>{}</code> — <b>{}</b>{}",
            if o.intent.side == Side::Buy { "BUY" } else { "SELL" },
            esc(&short(&o.intent.mint.to_string())),
            esc(&short(&o.id.to_string())),
            o.state.as_str(),
            o.signed.as_ref().map(|s| format!(" · sig <code>{}</code>", esc(&short(&s.signature)))).unwrap_or_default()
        ));
    }
    reply(st, chat_id, text, json!(null)).await;
    Ok(())
}

async fn cmd_settings(st: &AppState, user: &User, chat_id: i64) -> Result<(), ServiceError> {
    let s = service::settings(st, user).await?;
    let presets: Vec<String> = s.buy_presets.iter().map(|l| render::sol(l.0)).collect();
    let text = format!(
        "<b>Settings</b>\nBuy presets: {} SOL\nSell presets: {}%\nSlippage: {} bps\nPriority price: {} µlamports/CU · max priority fee {} SOL\nTip: {} SOL\nConfirm every trade: {}\nSenders: {:?}\n\nChange with /set: <code>/set slippage 300</code>, <code>/set presets 0.05,0.1,0.25</code>, <code>/set confirm on|off</code>, <code>/set priority 20000</code>, <code>/set maxfee 0.0002</code>",
        esc(&presets.join(", ")),
        s.sell_presets_bps.iter().map(|b| (b / 100).to_string()).collect::<Vec<_>>().join(", "),
        s.slippage.get(),
        s.compute_unit_price_micro_lamports,
        render::sol(s.max_priority_fee.0),
        render::sol(s.tip.0),
        if s.confirm_trades { "on" } else { "off" },
        s.senders
    );
    reply(st, chat_id, text, json!(null)).await;
    Ok(())
}

async fn cmd_set(st: &AppState, user: &User, chat_id: i64, args: &[String]) -> Result<(), ServiceError> {
    let (Some(k), Some(v)) = (args.first(), args.get(1)) else { return Err(ServiceError::BadRequest("usage: /set <key> <value>".into())) };
    let mut s = service::settings(st, user).await?;
    let bad = |m: &str| ServiceError::BadRequest(m.to_string());
    match k.as_str() {
        "slippage" => s.slippage = Bps::new(v.parse().map_err(|_| bad("slippage in bps"))?).map_err(|e| bad(&e.to_string()))?,
        "presets" => s.buy_presets = v.split(',').map(|x| Lamports::from_sol_str(x)).collect::<Result<Vec<_>, _>>().map_err(|e| bad(&e.to_string()))?,
        "confirm" => s.confirm_trades = matches!(v.as_str(), "on" | "true" | "yes"),
        "priority" => s.compute_unit_price_micro_lamports = v.parse().map_err(|_| bad("µlamports per CU"))?,
        "maxfee" => s.max_priority_fee = Lamports::from_sol_str(v).map_err(|e| bad(&e.to_string()))?,
        "tip" => s.tip = Lamports::from_sol_str(v).map_err(|e| bad(&e.to_string()))?,
        _ => return Err(bad("unknown setting")),
    }
    s.validate().map_err(ServiceError::BadRequest)?;
    st.app.set_settings(user.id, &s).await?;
    st.app.audit(Some(user.id), "settings_changed", json!({"key": k})).await.ok();
    reply(st, chat_id, "✅ Saved. Per-wallet hard limits (max order, daily turnover, fee/tip caps) still apply.".into(), json!(null)).await;
    Ok(())
}

async fn cmd_track(st: &AppState, user: &User, chat_id: i64, args: &[String], add: bool) -> Result<(), ServiceError> {
    let addr = args.first().ok_or_else(|| ServiceError::BadRequest("usage: /track <wallet address>".into()))?;
    let pk = service::parse_mint_arg(addr)?;
    if add {
        st.app
            .add_tracked(&crate::appstore::TrackedWallet { user_id: user.id, address: pk.to_string(), label: args.get(1).cloned(), created_at: chrono::Utc::now() })
            .await?;
        reply(st, chat_id, format!("👀 Tracking <code>{}</code> (public on-chain activity only). Live alerts: polling-based, P1 — see /status.", esc(&pk.to_string())), json!(null)).await;
    } else {
        let removed = st.app.remove_tracked(user.id, &pk.to_string()).await?;
        reply(st, chat_id, if removed { "Removed.".into() } else { "Not tracked.".into() }, json!(null)).await;
    }
    Ok(())
}

async fn cmd_pause(st: &AppState, user: &User, chat_id: i64, pause: bool) -> Result<(), ServiceError> {
    let w = service::default_wallet(st, user).await?.ok_or(ServiceError::NoWallet)?;
    let mut p = st.app.get_policy(w.id).await?.unwrap_or_else(|| crate::providers::default_policy(st.cfg.mode, &st.senders));
    p.paused = pause;
    st.app.set_policy(w.id, &p).await?;
    st.app.audit(Some(user.id), if pause { "paused" } else { "resumed" }, json!({"wallet_id": w.id})).await.ok();
    reply(st, chat_id, if pause { "⏸ Trading paused for your wallet. Pending signed transactions may still land; nothing new will be signed.".into() } else { "▶️ Trading resumed.".into() }, json!(null)).await;
    Ok(())
}

async fn handle_callback(st: &AppState, cb: &Value) {
    let cb_id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let Some(from_id) = cb.pointer("/from/id").and_then(|v| v.as_i64()) else { return };
    let chat_id = cb.pointer("/message/chat/id").and_then(|v| v.as_i64()).unwrap_or(0);
    let data = cb.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let answer = |text: String| {
        let (tg, id) = (st.tg.clone(), cb_id.clone());
        async move {
            if let Some(tg) = tg {
                let _ = tg.answer_callback(&id, &text).await;
            }
        }
    };
    let Some(token) = data.strip_prefix("t:") else { return answer("Unknown button".into()).await };
    let check = match st.app.check_callback(token, from_id, chat_id).await {
        Ok(c) => c,
        Err(_) => return answer("Try again".into()).await,
    };
    let user = match st.app.user_by_telegram(from_id).await {
        Ok(Some(u)) => u,
        _ => return answer("Send /start first".into()).await,
    };
    let (action, replayed) = match check {
        CallbackCheck::Ok(a) => (a, false),
        CallbackCheck::Replayed(a) => (a, true),
        CallbackCheck::Expired => return answer("This button expired. Paste the mint again for a fresh quote.".into()).await,
        CallbackCheck::WrongUser => {
            st.app.audit(Some(user.id), "callback_wrong_user", json!({"token": short(token)})).await.ok();
            return answer("Not your button.".into()).await;
        }
        CallbackCheck::Unknown => return answer("Unknown or expired button.".into()).await,
    };
    // Answer promptly (Telegram shows a spinner until answered).
    answer(if replayed { "Already received — not submitting twice.".into() } else { "Working…".into() }).await;
    let result: Result<(), ServiceError> = async {
        match action {
            CallbackAction::Buy { mint, lamports } => {
                let w = service::default_wallet(st, &user).await?.ok_or(ServiceError::NoWallet)?;
                let intent = service::build_intent(st, &user, &w, Side::Buy, service::parse_mint_arg(&mint)?, AmountSpec::ExactSolIn { lamports: Lamports(lamports) }, format!("tg:cb:{token}"), None).await?;
                let o = service::submit(st, &user, intent, None).await?;
                if !replayed {
                    reply(st, chat_id, format!("⏳ Buy order <code>{}</code> created — preparing quote…", o.id), json!(null)).await;
                }
                Ok(())
            }
            CallbackAction::Sell { mint, bps } => {
                let w = service::default_wallet(st, &user).await?.ok_or(ServiceError::NoWallet)?;
                let intent = service::build_intent(st, &user, &w, Side::Sell, service::parse_mint_arg(&mint)?, AmountSpec::SellPercent { bps: Bps::new(bps as u32).map_err(|e| ServiceError::BadRequest(e.to_string()))? }, format!("tg:cb:{token}"), None).await?;
                let o = service::submit(st, &user, intent, None).await?;
                if !replayed {
                    reply(st, chat_id, format!("⏳ Sell order <code>{}</code> created — preparing quote…", o.id), json!(null)).await;
                }
                Ok(())
            }
            CallbackAction::Approve { order_id } => {
                // Engine approval is idempotent (state CAS), replays are harmless.
                st.engine.approve(order_id, user.id).await?;
                Ok(())
            }
            CallbackAction::Cancel { order_id } => {
                let o = st.engine.cancel(order_id, user.id).await?;
                reply(st, chat_id, format!("Order <code>{}</code>: {}", o.id, trader_domain::user_status(o.state)), json!(null)).await;
                Ok(())
            }
            CallbackAction::Refresh { mint } => token_panel(st, &user, chat_id, &mint).await,
            CallbackAction::CreateWallet => {
                let w = service::create_wallet(st, &user).await?;
                reply(st, chat_id, format!("🔑 Wallet ready: <code>{}</code>\nFund it with a small amount only. /wallet for balance.", esc(&w.pubkey)), json!(null)).await;
                Ok(())
            }
            CallbackAction::MockAirdrop => {
                if replayed {
                    return Ok(());
                }
                let l = service::airdrop_mock(st, &user).await?;
                reply(st, chat_id, format!("💧 Requested {} test SOL (not real funds).", render::sol(l.0)), json!(null)).await;
                Ok(())
            }
        }
    }
    .await;
    if let Err(e) = result {
        reply(st, chat_id, format!("❌ {}", esc(&e.to_string())), json!(null)).await;
    }
}
