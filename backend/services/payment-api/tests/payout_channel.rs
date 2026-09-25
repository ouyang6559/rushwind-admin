//! Real HTTP round-trips for the live payout adapters (`spec/04` §9) against
//! a one-shot loopback mock upstream — no database, no live gateway. Each test
//! drives a [`Mgzf`] / [`Yibao`] `PayoutExec` through the actual `reqwest`
//! stack, asserts the request the adapter put on the wire (sign / body), and
//! checks the canned answer normalises to the right [`ExecResp`] status.
//!
//! The pure request-building / response-parsing helpers are covered inline in
//! the channel modules; this file proves the transport + wiring end to end.

#![allow(clippy::unwrap_used)]

use std::io::Read;
use std::io::Write;

use payment_api::data::payout_orders;
use payment_api::payout::channel::mgzf::Mgzf;
use payment_api::payout::channel::yibao::Yibao;
use payment_api::payout::exec::{PayoutChannelCfg, PayoutExec};

/// A one-shot loopback server: accept a single connection, drain the request
/// (headers + Content-Length body), answer `reply` as a JSON 200, and hand the
/// raw request bytes back through the join handle for assertions.
fn mock_upstream(reply: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock bind");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("mock accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read through the end of the header block.
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        // Then drain exactly Content-Length bytes of body (best effort).
        let headers = String::from_utf8_lossy(&buf).to_lowercase();
        let want = headers
            .split("content-length:")
            .nth(1)
            .and_then(|s| {
                s.trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);
        let have = buf.len().saturating_sub(
            buf.windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| p + 4)
                .unwrap_or(buf.len()),
        );
        let mut missing = want.saturating_sub(have);
        while missing > 0 {
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    missing = missing.saturating_sub(n);
                }
            }
        }
        let resp = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            reply.len(),
            reply
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        buf
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn order() -> payout_orders::Model {
    payout_orders::Model {
        order_no: "P0922001230000101".into(),
        user_id: 7,
        money: 100 * 10_000, // 100元
        cardnumber: Some("622202000000001".into()),
        accountname: Some("张三".into()),
        bankname: Some("工商银行".into()),
        province: Some("北京".into()),
        city: Some("北京".into()),
        additional: Some(r#"{"bankProvinceCode":"110000"}"#.into()),
        ..Default::default()
    }
}

fn chan(base: &str) -> PayoutChannelCfg {
    PayoutChannelCfg {
        id: 3,
        code: "MGZF".into(),
        name: "蘑菇代付".into(),
        mch_id: Some("MG888".into()),
        sign_key: "SECRET".into(),
        app_secret: "PWD".into(),
        exec_gateway: base.into(),
        query_gateway: base.into(),
        ..Default::default()
    }
}

fn req_str(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

#[tokio::test]
async fn mgzf_exec_posts_signed_form_and_maps_processing() {
    let (base, handle) = mock_upstream(r#"{"code":"0000","status":"1","msg":"accepted"}"#);
    let resp = Mgzf::new().exec(&order(), &chan(&base)).await.unwrap();
    assert_eq!(resp.status, 1, "code 0000 + status 1 → 处理中");
    assert_eq!(resp.msg, "accepted");

    let req = req_str(&handle.join().unwrap());
    assert!(req.contains("POST"), "form POST: {req}");
    assert!(req.contains("method=settle"), "carry method=settle");
    assert!(req.contains("amount=10000"), "amount in 分: {req}");
    assert!(req.contains("sign="), "signed request carries sign=");
}

#[tokio::test]
async fn mgzf_query_settles_success_and_processing() {
    let (base, handle) = mock_upstream(r#"{"code":"0000","status":"1","msg":"paid"}"#);
    let resp = Mgzf::new().query(&order(), &chan(&base)).await.unwrap();
    assert_eq!(resp.status, 2, "§9.2 query status 1 → 成功");
    assert_eq!(resp.msg, "paid");
    let req = req_str(&handle.join().unwrap());
    assert!(req.contains("method=settlequery"), "query method");
    assert!(req.contains("out_trade_no="), "query keyed on out_trade_no");
}

#[tokio::test]
async fn mgzf_transport_failure_is_upstream_error() {
    // A non-2xx answer is surfaced as Err (§8.2 result===FALSE → no fold).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
        let _ = sock.write_all(b"HTTP/1.0 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
        let _ = sock.flush();
    });
    let base = format!("http://127.0.0.1:{port}");
    let err = Mgzf::new().exec(&order(), &chan(&base)).await;
    assert!(err.is_err(), "502 must fail the drive");
    handle.join().unwrap();
}

#[tokio::test]
async fn yibao_exec_posts_json_with_api_sign() {
    let (base, handle) = mock_upstream(r#"{"status":0,"message":"ok"}"#);
    let resp = Yibao::new().exec(&order(), &chan(&base)).await.unwrap();
    assert_eq!(resp.status, 1, "status 0 → 处理中");

    let req = req_str(&handle.join().unwrap());
    assert!(req.contains("POST /withdraw/create"), "path joined: {req}");
    // Header names are lowercased on the wire (RFC 7230 case-insensitive); the
    // legacy's `Api-Sign` reaches the upstream equivalently.
    assert!(
        req.to_lowercase().contains("api-sign:"),
        "Api-Sign header present"
    );
    assert!(
        req.contains("\"merchantId\":\"MG888\""),
        "json body merchantId"
    );
    assert!(req.contains("\"amount\":103.0"), "amount = realAmount + 3");
}

#[tokio::test]
async fn yibao_query_reports_paid_as_processing_faithfully() {
    let (base, handle) = mock_upstream(r#"{"status":0,"body":{"status":1,"message":"付款成功"}}"#);
    let resp = Yibao::new().query(&order(), &chan(&base)).await.unwrap();
    // §9.2 quirk: a paid Yibao answer is reported 处理中 (never terminal 2).
    assert_eq!(resp.status, 1);
    assert_eq!(resp.msg, "付款成功");
    let req = req_str(&handle.join().unwrap());
    assert!(req.contains("POST /withdraw/query"), "query path joined");
    assert!(
        req.contains("\"orderId\":\"P0922001230000101\""),
        "orderId in body"
    );
}
