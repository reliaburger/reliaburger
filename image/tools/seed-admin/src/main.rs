// Spike-only: add a pre-hashed admin token to a `relish init` bootstrap
// (what quickstart's security::prepare does), print the plaintext once.
use reliaburger::sesame::{token::create_token, types::{ApiRole, SecurityState, TokenScope}};
fn main() {
    let path = std::env::args().nth(1).expect("usage: seed-admin <cluster>-security-bootstrap.json");
    let mut state: SecurityState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let t = create_token("laptop-admin", ApiRole::Admin, TokenScope::default(), None).unwrap();
    state.api_tokens.push(t.token);
    std::fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    println!("{}", t.plaintext);
}
