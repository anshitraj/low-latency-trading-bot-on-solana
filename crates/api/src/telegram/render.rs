//! Message rendering (HTML). All dynamic values pass through `esc`.

pub use super::client::esc;
use trader_domain::money::format_units;
use trader_domain::{user_status_with_fill, Network, OrderState, Quote, Side};
use trader_orders::{FillRecord, Order};

pub fn short(s: &str) -> String {
    if s.len() <= 12 {
        s.to_string()
    } else {
        format!("{}…{}", &s[..5], &s[s.len() - 5..])
    }
}

pub fn sol(l: u64) -> String {
    format_units(l, 9)
}

pub fn explorer(network: Network, sig: &str) -> Option<String> {
    match network {
        Network::Mainnet => Some(format!("https://explorer.solana.com/tx/{sig}")),
        Network::Devnet => Some(format!(
            "https://explorer.solana.com/tx/{sig}?cluster=devnet"
        )),
        _ => None,
    }
}

pub fn mode_banner(n: Network) -> String {
    match n {
        Network::Mock => {
            "🧪 <b>MOCK MODE</b> — simulated chain, not real funds or prices.\n".into()
        }
        Network::Localnet => "🧪 <b>LOCALNET</b> — local test validator, not real funds.\n".into(),
        Network::Devnet => "🧪 <b>DEVNET</b> — test network, tokens have no value.\n".into(),
        Network::Testnet => "🧪 <b>TESTNET</b>\n".into(),
        Network::Mainnet => "⚠️ <b>MAINNET</b> — real funds.\n".into(),
    }
}

pub fn quote_block(q: &Quote) -> String {
    let (inp, out) = match q.side {
        Side::Buy => (
            format!("{} SOL", sol(q.in_amount)),
            format!("{} tokens", format_units(q.expected_out, q.out_decimals)),
        ),
        Side::Sell => (
            format!("{} tokens", format_units(q.in_amount, q.in_decimals)),
            format!("{} SOL", sol(q.expected_out)),
        ),
    };
    let min = match q.side {
        Side::Buy => format!("{} tokens", format_units(q.min_out, q.out_decimals)),
        Side::Sell => format!("{} SOL", sol(q.min_out)),
    };
    let impact = q
        .price_impact_bps
        .map(|b| format!("{}.{:02}%", b / 100, b % 100))
        .unwrap_or_else(|| "unknown".into());
    let src = match &q.source {
        trader_domain::QuoteSource::OnChain => format!(
            "on-chain state @ slot {} ({} ms old)",
            q.context_slot, q.state_age_ms
        ),
        trader_domain::QuoteSource::ExternalApi { provider } => {
            format!("external: {}", esc(provider))
        }
        trader_domain::QuoteSource::Mock => "MOCK pool (not real)".into(),
    };
    let mut s = format!(
        "{} <b>{}</b> → ~<b>{}</b>\nMin received: {}\nPrice impact: {}\nNetwork fee: {} SOL · priority ≤ {} SOL · tip {} SOL\nProtocol/creator fee (est.): {} SOL\nRent deposit (refundable on close): {} SOL\n<b>Max total spend: {} SOL</b>\nQuote source: {}",
        if q.side == Side::Buy { "Buy" } else { "Sell" },
        esc(&inp),
        esc(&out),
        esc(&min),
        impact,
        sol(q.costs.base_fee.0),
        sol(q.costs.priority_fee.0),
        sol(q.costs.tip.0),
        sol(q.costs.protocol_fee.0),
        sol(q.costs.rent_deposit.0),
        sol(q.max_total_debit.0),
        src
    );
    for w in &q.warnings {
        s.push_str(&format!("\n⚠️ {}", esc(w)));
    }
    s
}

pub fn fill_block(f: &FillRecord, side: Side) -> String {
    let dec = f.token_decimals.unwrap_or(6);
    let tok = format_units(
        f.token_delta_raw.unsigned_abs().min(u64::MAX as u128) as u64,
        dec,
    );
    let sold = f.sol_delta_lamports.unsigned_abs().min(u64::MAX as u128) as u64;
    match (f.success, side) {
        (true, Side::Buy) => format!("Received <b>{tok}</b> tokens · SOL debited {} (incl. fees/rent) · network fee {} SOL · slot {}", sol(sold), sol(f.fee_lamports), f.slot),
        (true, Side::Sell) => format!("Sold <b>{tok}</b> tokens · SOL credited {} (net of fees) · network fee {} SOL · slot {}", sol(sold), sol(f.fee_lamports), f.slot),
        (false, _) => format!("No swap executed. Network fee paid: {} SOL. Error: <code>{}</code>", sol(f.fee_lamports), esc(f.error.as_deref().unwrap_or("unknown"))),
    }
}

pub fn order_status(o: &Order, network: Network, detail: &str) -> String {
    let side = o.intent.side;
    let mut s = format!(
        "{}<b>{} {}</b> · <code>{}</code>\nStatus: <b>{}</b> — {}",
        mode_banner(network),
        if side == Side::Buy { "BUY" } else { "SELL" },
        esc(&short(&o.intent.mint.to_string())),
        o.state.as_str(),
        user_status_with_fill(o.state, o.fill.is_some()),
        esc(detail)
    );
    if let Some(sig) = o.signed.as_ref().map(|s| s.signature.clone()) {
        s.push_str(&format!("\nSignature: <code>{}</code>", esc(&sig)));
        if let Some(url) = explorer(network, &sig) {
            s.push_str(&format!("\n<a href=\"{}\">View on explorer</a>", esc(&url)));
        }
    }
    if matches!(
        o.state,
        OrderState::Confirmed | OrderState::Finalized | OrderState::FailedOnchain
    ) {
        if let Some(f) = &o.fill {
            s.push('\n');
            s.push_str(&fill_block(f, side));
        }
    }
    if o.state == OrderState::AwaitingAuth {
        if let Some(q) = &o.quote {
            s.push_str("\n\n");
            s.push_str(&quote_block(q));
            s.push_str("\n\nConfirm within the quote validity window. Slippage protection is enforced on-chain.");
        }
    }
    if let Some(r) = &o.reject_reason {
        if o.state == OrderState::Rejected {
            s.push_str(&format!("\nReason: {}", esc(r)));
        }
    }
    s
}

pub const HELP: &str = "<b>Commands</b>
/start – intro & setup
/wallet – your trading wallet
/balance – SOL balance
/buy &lt;mint&gt; &lt;SOL&gt; – buy (e.g. /buy So1…12 0.1)
/sell &lt;mint&gt; &lt;percent&gt; – sell % of balance
/positions – open positions & PnL
/orders – recent orders
/settings – presets, slippage, fees (/set key value)
/track &lt;wallet&gt; · /untrack &lt;wallet&gt; – public wallet watchlist
/pause · /resume – kill switch
/status – service & provider health
/copy · /referrals · /export · /support

Paste a token mint address to get a token panel with buy/sell buttons.";

pub const INTRO: &str = "This bot builds, signs and submits Solana swaps for you from a dedicated <b>test trading wallet</b> held by this service (custodial, private beta). Every trade shows the max total spend before you confirm, and success is reported only after on-chain confirmation.

<b>Risks:</b> memecoins can go to zero; failed transactions still pay network fees; quotes can move; this service has not had a security audit. Never fund the wallet with more than you can lose. We never ask for your seed phrase.";
