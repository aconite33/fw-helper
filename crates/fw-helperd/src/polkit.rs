//! polkit authorization.
//!
//! D-Bus policy is too coarse to express "this user may set a charge limit but not
//! a power limit", so authorization happens here, per action, at call time
//! (ADR 0003). Every method that changes hardware must go through [`check`].

use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use std::time::Duration;

/// Subject type understood by polkit for a D-Bus caller.
const SUBJECT_KIND: &str = "system-bus-name";
const FLAG_NONE: u32 = 0;
/// `AllowUserInteraction` — lets polkit prompt rather than refusing outright.
const FLAG_ALLOW_INTERACTION: u32 = 1;

/// Upper bound on how long we will wait for a human to answer a polkit prompt.
///
/// This is not tuning. **polkit blocks indefinitely when no authentication agent
/// can service the request** — which happens for any caller not attached to a login
/// session, such as a shell with no `XDG_SESSION_ID`. Without a bound, one such call
/// wedges the method handler forever.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

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

    // Ask without interaction first. This always returns promptly, and covers the
    // already-authorized cases: root, an admin whose auth_admin_keep is still
    // valid, or a permissive local rule. Only if that fails do we risk a prompt.
    let (authorized, _challenge, _) = authority
        .check_authorization(&subject, action, HashMap::new(), FLAG_NONE, "")
        .await
        .map_err(|e| format!("polkit check failed: {e}"))?;
    if authorized {
        return Ok(());
    }

    // Authentication is genuinely required. Now allow a prompt, bounded.
    let prompt =
        authority.check_authorization(&subject, action, HashMap::new(), FLAG_ALLOW_INTERACTION, "");
    match tokio::time::timeout(PROMPT_TIMEOUT, prompt).await {
        Ok(Ok((true, _, _))) => Ok(()),
        Ok(Ok((false, _, _))) => Err(format!("not authorized for {action}")),
        Ok(Err(e)) => Err(format!("polkit check failed: {e}")),
        Err(_) => Err(format!(
            "no answer from polkit within {}s for {action}. No authentication agent \
             is available for this caller — run from a desktop session, or as root",
            PROMPT_TIMEOUT.as_secs()
        )),
    }
}

pub mod actions {
    pub const SET_CHARGE_LIMIT: &str = "org.fwhelper.set-charge-limit";
    pub const SET_FAN: &str = "org.fwhelper.set-fan";
    pub const SET_POWER_LIMIT: &str = "org.fwhelper.set-power-limit";
}
