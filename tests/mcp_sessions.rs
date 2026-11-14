//! Exercise the bounded session-store seam without a 30-minute HTTP wait.
use std::time::Duration;
use tokomotive::mcp::Sessions;
#[test]
fn expires_idle_sessions_and_enforces_global_and_key_bounds() {
    let sessions = Sessions::with_limits(Duration::from_secs(1800), 2, 1);
    let first = sessions.create(1, "alpha").unwrap();
    assert!(sessions.create(1, "beta").is_none());
    assert!(sessions.create(2, "alpha").is_some());
    assert!(sessions.create(3, "alpha").is_none());
    assert!(!sessions.check(&first, 2, "alpha", true));
    assert!(!sessions.check(&first, 1, "beta", true));
    assert!(sessions.check(&first, 1, "alpha", true));
    let expired = Sessions::with_limits(Duration::ZERO, 1, 1);
    let first = expired.create(1, "alpha").unwrap();
    assert!(!expired.check(&first, 1, "alpha", true));
    assert!(expired.create(3, "alpha").is_some());
}
