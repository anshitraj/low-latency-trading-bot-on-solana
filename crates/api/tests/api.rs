//! API + Telegram integration tests in MOCK mode with a mock Telegram Bot API.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use trader_api::auth::sign_init_data;
use trader_api::config::Config;
use trader_api::{routes, spawn_background, state};

const BOT: &str = "123456:TEST-bot-token-not-real";
const SECRET: &str = "test-webhook-secret-0123456789abcdef";

#[derive(Clone, Default)]
struct TgMock {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    next_id: Arc<AtomicI64>,
    fail_429_once: Arc<Mutex<bool>>,
}

async fn tg_handler(
    State(m): State<TgMock>,
    Path((_bot, method)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    if method == "sendMessage" {
        let mut f = m.fail_429_once.lock();
        if *f {
            *f = false;
            return Json(
                json!({"ok": false, "error_code": 429, "description": "Too Many Requests", "parameters": {"retry_after": 1}}),
            );
        }
    }
    m.calls.lock().push((method.clone(), body));
    let id = m.next_id.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({"ok": true, "result": {"message_id": id}}))
}

struct T {
    app: Router,
    tg: TgMock,
    st: state::AppState,
    update_id: AtomicI64,
}

async fn setup(allowlist: Vec<i64>) -> T {
    let tg = TgMock::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let r = Router::new()
        .route("/{bot}/{method}", post(tg_handler))
        .with_state(tg.clone());
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    let mut cfg = Config::mock();
    cfg.bot_token = Some(BOT.into());
    cfg.webhook_secret = Some(SECRET.into());
    cfg.telegram_api_base = base;
    cfg.beta_allowlist = allowlist;
    let built = state::build(cfg).await.unwrap();
    let st = spawn_background(built).await;
    T {
        app: routes::router(st.clone()),
        tg,
        st,
        update_id: AtomicI64::new(1000),
    }
}

impl T {
    async fn call(&self, req: Request<Body>) -> (StatusCode, Value) {
        let r = self.app.clone().oneshot(req).await.unwrap();
        let s = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }
    async fn webhook(&self, upd: Value, secret: Option<&str>) -> StatusCode {
        let mut b = Request::post("/telegram/webhook").header("content-type", "application/json");
        if let Some(s) = secret {
            b = b.header("x-telegram-bot-api-secret-token", s);
        }
        self.call(b.body(Body::from(upd.to_string())).unwrap())
            .await
            .0
    }
    fn next_update(&self) -> i64 {
        self.update_id.fetch_add(1, Ordering::SeqCst)
    }
    async fn text(&self, from: i64, text: &str) {
        let u = json!({"update_id": self.next_update(), "message": {"message_id": 1, "from": {"id": from, "is_bot": false, "first_name": "T"}, "chat": {"id": from, "type": "private"}, "date": 0, "text": text}});
        assert_eq!(self.webhook(u, Some(SECRET)).await, StatusCode::OK);
    }
    async fn press(&self, from: i64, chat: i64, data: &str) {
        let u = json!({"update_id": self.next_update(), "callback_query": {"id": format!("cb{}", self.next_update()), "from": {"id": from, "is_bot": false, "first_name": "T"}, "message": {"message_id": 5, "chat": {"id": chat, "type": "private"}}, "data": data}});
        assert_eq!(self.webhook(u, Some(SECRET)).await, StatusCode::OK);
    }
    /// Wait until some sent message to `chat` contains `needle`; return it.
    async fn wait_msg(&self, chat: i64, needle: &str) -> Value {
        for _ in 0..400 {
            if let Some((_, b)) = self.tg.calls.lock().iter().rev().find(|(m, b)| {
                (m == "sendMessage" || m == "editMessageText")
                    && b["chat_id"] == chat
                    && b["text"].as_str().unwrap_or("").contains(needle)
            }) {
                return b.clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let all: Vec<String> = self
            .tg
            .calls
            .lock()
            .iter()
            .map(|(m, b)| {
                format!(
                    "{m}: {}",
                    b["text"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .collect::<String>()
                )
            })
            .collect();
        panic!(
            "no message containing {needle:?} to {chat}; got:\n{}",
            all.join("\n")
        );
    }
    fn button(msg: &Value, label_contains: &str) -> String {
        for row in msg["reply_markup"]["inline_keyboard"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            for b in row.as_array().cloned().unwrap_or_default() {
                if b["text"].as_str().unwrap_or("").contains(label_contains) {
                    return b["callback_data"].as_str().unwrap().to_string();
                }
            }
        }
        panic!("button {label_contains:?} not in {msg}");
    }
    fn init_data(&self, user_id: i64, auth_date: i64) -> String {
        let pairs = vec![
            ("auth_date".to_string(), auth_date.to_string()),
            ("query_id".to_string(), format!("Q{user_id}{auth_date}")),
            (
                "user".to_string(),
                format!(r#"{{"id":{user_id},"first_name":"M"}}"#),
            ),
        ];
        let hash = sign_init_data(BOT, &pairs);
        let mut s = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &pairs {
            s.append_pair(k, v);
        }
        s.append_pair("hash", &hash);
        s.finish()
    }
    async fn session(&self, user_id: i64) -> String {
        let (s, v) = self.call(Request::post("/v1/auth/telegram").header("content-type", "application/json").body(Body::from(json!({"init_data": self.init_data(user_id, chrono::Utc::now().timestamp())}).to_string())).unwrap()).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["token"].as_str().unwrap().to_string()
    }
    async fn authed(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
        idem: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json");
        if let Some(k) = idem {
            b = b.header("idempotency-key", k);
        }
        self.call(
            b.body(
                body.map(|v| Body::from(v.to_string()))
                    .unwrap_or(Body::empty()),
            )
            .unwrap(),
        )
        .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_secret_and_update_dedupe() {
    let t = setup(vec![]).await;
    let u = json!({"update_id": 1, "message": {"message_id": 1, "from": {"id": 7, "is_bot": false}, "chat": {"id": 7, "type": "private"}, "text": "/help"}});
    assert_eq!(t.webhook(u.clone(), None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        t.webhook(u.clone(), Some("wrong-secret-wrong-secret-wrong"))
            .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(t.webhook(u.clone(), Some(SECRET)).await, StatusCode::OK);
    assert_eq!(
        t.webhook(u.clone(), Some(SECRET)).await,
        StatusCode::OK,
        "duplicate acked"
    );
    t.wait_msg(7, "Commands").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n =
        t.tg.calls
            .lock()
            .iter()
            .filter(|(_, b)| b["text"].as_str().unwrap_or("").contains("Commands"))
            .count();
    assert_eq!(n, 1, "duplicate update processed once");
    // malformed body
    let r = t
        .call(
            Request::post("/telegram/webhook")
                .header("x-telegram-bot-api-secret-token", SECRET)
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.0, StatusCode::BAD_REQUEST);
    // group chats never trade
    let g = json!({"update_id": 2, "message": {"message_id": 1, "from": {"id": 8}, "chat": {"id": -100, "type": "group"}, "text": "/start"}});
    assert_eq!(t.webhook(g, Some(SECRET)).await, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn telegram_full_flow_buy_confirm_filled_with_replays_and_spoofing() {
    let t = setup(vec![]).await;
    let user = 4242i64;
    let attacker = 6666i64;
    t.text(user, "/start").await;
    let intro = t.wait_msg(user, "test trading wallet").await;
    let create = T::button(&intro, "Create test trading wallet");
    t.press(user, user, &create).await;
    t.wait_msg(user, "Wallet ready").await;
    t.text(user, "/wallet").await;
    let w = t.wait_msg(user, "Balance").await;
    let air = T::button(&w, "Airdrop");
    t.press(user, user, &air).await;
    t.wait_msg(user, "test SOL").await;

    let mint = t.st.mock.as_ref().unwrap().demo_mint.to_string();
    t.text(user, &mint).await;
    let panel = t.wait_msg(user, "Market: mock-dex").await;
    assert!(panel["text"].as_str().unwrap().contains("MOCK MODE"));
    let buy = T::button(&panel, "Buy 0.05");

    // attacker presses the victim's button (in their own chat) -> refused
    t.press(attacker, attacker, &buy).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        t.st.orders
            .list_orders(
                t.st.app.user_by_telegram(user).await.unwrap().unwrap().id,
                10
            )
            .await
            .unwrap()
            .len(),
        0
    );

    // double-tap the buy button: one order only
    t.press(user, user, &buy).await;
    t.press(user, user, &buy).await;
    let quote_msg = t.wait_msg(user, "AWAITING_AUTH").await;
    assert!(quote_msg["text"]
        .as_str()
        .unwrap()
        .contains("Max total spend"));
    let confirm = T::button(&quote_msg, "Confirm");
    // confirm twice (replay) + attacker confirm
    t.press(attacker, attacker, &confirm).await;
    t.press(user, user, &confirm).await;
    t.press(user, user, &confirm).await;
    let filled = t.wait_msg(user, "filled (finalized)").await;
    assert!(filled["text"].as_str().unwrap().contains("Received"));
    let uid = t.st.app.user_by_telegram(user).await.unwrap().unwrap().id;
    let orders = t.st.orders.list_orders(uid, 10).await.unwrap();
    assert_eq!(orders.len(), 1, "double tap produced one order");
    assert_eq!(orders[0].state, trader_domain::OrderState::Finalized);
    assert_eq!(
        t.st.mock.as_ref().unwrap().chain.landed_ok().len(),
        1,
        "exactly one swap on-chain"
    );
    // never told "filled" before confirmation: any message with 'filled' must be CONFIRMED/FINALIZED
    for (_, b) in t.tg.calls.lock().iter() {
        let s = b["text"].as_str().unwrap_or("");
        if s.contains("filled") {
            assert!(s.contains("CONFIRMED") || s.contains("FINALIZED"), "{s}");
        }
    }
    t.text(user, "/positions").await;
    t.wait_msg(user, "weighted average").await;
    t.text(user, "/export").await;
    t.wait_msg(user, "disabled").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn beta_allowlist_blocks_strangers() {
    let t = setup(vec![1]).await;
    t.text(99, "/start").await;
    t.wait_msg(99, "closed private beta").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn miniapp_auth_replay_tamper_expiry_and_isolation() {
    let t = setup(vec![]).await;
    let now = chrono::Utc::now().timestamp();
    let d = t.init_data(5, now);
    let req = |d: &str| {
        Request::post("/v1/auth/telegram")
            .header("content-type", "application/json")
            .body(Body::from(json!({"init_data": d}).to_string()))
            .unwrap()
    };
    let (s, v) = t.call(req(&d)).await;
    assert_eq!(s, StatusCode::OK);
    let tok_a = v["token"].as_str().unwrap().to_string();
    assert_eq!(
        t.call(req(&d)).await.0,
        StatusCode::UNAUTHORIZED,
        "same initData cannot mint a second session"
    );
    assert_eq!(
        t.call(req(&d.replace("%22id%22%3A5", "%22id%22%3A6")))
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "tampered"
    );
    assert_eq!(
        t.call(req(&t.init_data(5, now - 10_000))).await.0,
        StatusCode::UNAUTHORIZED,
        "expired auth_date"
    );
    assert_eq!(
        t.authed("GET", "/v1/me", "garbage", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let r = t
        .call(Request::get("/v1/me").body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.0, StatusCode::UNAUTHORIZED);

    // user A creates wallet, airdrops, creates order via API
    assert_eq!(
        t.authed("POST", "/v1/wallets", &tok_a, None, None).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        t.authed("POST", "/v1/dev/airdrop", &tok_a, None, None)
            .await
            .0,
        StatusCode::OK
    );
    let mint = t.st.mock.as_ref().unwrap().demo_mint.to_string();
    let body = json!({"mint": mint, "side": "buy", "amount_sol": "0.1"});
    assert_eq!(
        t.authed("POST", "/v1/orders", &tok_a, Some(body.clone()), None)
            .await
            .0,
        StatusCode::BAD_REQUEST,
        "Idempotency-Key required"
    );
    let (s, v) = t
        .authed(
            "POST",
            "/v1/orders",
            &tok_a,
            Some(body.clone()),
            Some("mini-1"),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["order"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["order"]["state"], "AWAITING_AUTH");
    let (_, v2) = t
        .authed("POST", "/v1/orders", &tok_a, Some(body), Some("mini-1"))
        .await;
    assert_eq!(
        v2["order"]["id"].as_str().unwrap(),
        id,
        "same key -> same order"
    );

    // user B cannot see or approve A's order
    let tok_b = t.session(6).await;
    assert_eq!(
        t.authed("GET", &format!("/v1/orders/{id}"), &tok_b, None, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.authed(
            "POST",
            &format!("/v1/orders/{id}/approve"),
            &tok_b,
            None,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // A approves; wait for finality
    assert_eq!(
        t.authed(
            "POST",
            &format!("/v1/orders/{id}/approve"),
            &tok_a,
            None,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut state = String::new();
    for _ in 0..300 {
        let (_, o) = t
            .authed("GET", &format!("/v1/orders/{id}"), &tok_a, None, None)
            .await;
        state = o["order"]["state"].as_str().unwrap_or("").to_string();
        if state == "FINALIZED" && !o["order"]["fill"].is_null() {
            let sig = o["order"]["signed"]["signature"]
                .as_str()
                .unwrap()
                .to_string();
            let (s, tx) = t
                .authed(
                    "GET",
                    &format!("/v1/transactions/{sig}"),
                    &tok_a,
                    None,
                    None,
                )
                .await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(tx["state"], "FINALIZED");
            assert_eq!(
                t.authed(
                    "GET",
                    &format!("/v1/transactions/{sig}"),
                    &tok_b,
                    None,
                    None
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(state, "FINALIZED");
    let (_, p) = t.authed("GET", "/v1/portfolio", &tok_a, None, None).await;
    assert_eq!(p["cost_basis_method"], "weighted_average");
    let pos = &p["wallets"][0]["positions"][0];
    assert!(pos["qty_raw"].as_str().unwrap().parse::<u128>().unwrap() > 0);
    assert!(
        !pos["unrealized_pnl_lamports"].is_null(),
        "mock pool provides an executable sell quote"
    );

    // preview never persists
    let before =
        t.st.orders
            .list_orders(t.st.app.user_by_telegram(5).await.unwrap().unwrap().id, 100)
            .await
            .unwrap()
            .len();
    let (s, _) = t.authed("POST", "/v1/orders/preview", &tok_a, Some(json!({"mint": t.st.mock.as_ref().unwrap().demo_mint.to_string(), "side": "sell", "percent": 50})), None).await;
    assert_eq!(s, StatusCode::OK);
    let after =
        t.st.orders
            .list_orders(t.st.app.user_by_telegram(5).await.unwrap().unwrap().id, 100)
            .await
            .unwrap()
            .len();
    assert_eq!(before, after);
    // validation errors
    let (s, _) = t
        .authed(
            "POST",
            "/v1/orders/preview",
            &tok_a,
            Some(json!({"mint": "nope", "side": "buy", "amount_sol": "0.1"})),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.authed("POST", "/v1/orders/preview", &tok_a, Some(json!({"mint": t.st.mock.as_ref().unwrap().demo_mint.to_string(), "side": "buy", "amount_sol": "0.1e3"})), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "no float/exponent amounts");
    // feature-flagged endpoints are explicit
    assert_eq!(
        t.authed("GET", "/v1/copy-rules", &tok_a, None, None)
            .await
            .0,
        StatusCode::NOT_IMPLEMENTED
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_endpoints_are_private_and_telegram_429_retries() {
    let t = setup(vec![]).await;
    assert_eq!(
        t.call(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let r = t
        .app
        .clone()
        .oneshot(
            Request::get("/metrics")
                .header("authorization", "Bearer test-admin-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let (s, h) = t
        .call(Request::get("/v1/health").body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["mainnet_trading_enabled"], false);
    *t.tg.fail_429_once.lock() = true;
    t.text(77, "/help").await;
    t.wait_msg(77, "Commands").await; // delivered after retry_after
}
