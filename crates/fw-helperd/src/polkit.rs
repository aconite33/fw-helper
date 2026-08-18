//! polkit authorization.
//!
//! D-Bus policy is too coarse to express "this user may set a charge limit but not
//! a power limit", so authorization happens here, per action, at call time
//! (ADR 0003). Every method that changes hardware must go through [`check`].

use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

/// Subject type understood by polkit for a D-Bus caller.
const SUBJECT_KIND: &str = "system-bus-name";
/// `AllowUserInteraction` — lets polkit prompt rather than refusing outright.
const FLAG_ALLOW_INTERACTION: u32 = 1;

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Authority {
    #[allow(clippy::too_many_arguments)]
    fn check_authorization(
        &self,
        subject: &(&str, HashMap<&str, OwnedValue>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

/// Is `sender` allowed to perform `action`?
///
/// Fails **closed**: any error contacting polkit denies the request. A permissive
/// fallback here would mean an unreachable polkit silently granted everyone
/// hardware write access.
pub async fn check(conn: &zbus::Connection, sender: &str, action: &str) -> Result<(), String> {
    let authority = AuthorityProxy::new(conn)
        .await
        .map_err(|e| format!("cannot reach polkit: {e}"))?;

    let mut details = HashMap::new();
    let name = OwnedValue::try_from(zbus::zvariant::Value::from(sender))
        .map_err(|e| format!("cannot encode caller name: {e}"))?;
    details.insert("name", name);
    let subject = (SUBJECT_KIND, details);

    let (authorized, _challenge, _info) = authority
        .check_authorization(&subject, action, HashMap::new(), FLAG_ALLOW_INTERACTION, "")
        .await
        .map_err(|e| format!("polkit check failed: {e}"))?;

    if authorized {
        Ok(())
    } else {
        Err(format!("not authorized for {action}"))
    }
}

pub mod actions {
    pub const SET_CHARGE_LIMIT: &str = "org.fwhelper.set-charge-limit";
}
