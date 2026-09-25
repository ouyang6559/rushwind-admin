//! Merchant profile editing — the port of `User/AccountController::saveProfile`
//! (`spec/05` §10). Upstream the write is gated by an SMS (`auth_type = 0`) /
//! Google (`auth_type = 4`) second factor the Rust side has NOT wired yet, so
//! this slice ships the SERVICE LAYER only: a pure [`plan_profile`] that fixes
//! the whitelist / `agentname` / `birthday` rules (offline-testable, no DB) plus
//! [`apply_profile`] persisting the planned columns on the member row. The
//! panel HTTP write stays a documented seam until the second factor lands (see
//! the `敏感凭证管理接口的开发时序规范` rule).
//!
//! A faithful quirk worth calling out: legacy sets `$p['parentid']` from the
//! `agentname` reassignment and THEN strips it with the `allowField` whitelist
//! (parentid is not in it), so an agent swap never actually persists —
//! `agentname` only gates a "代理商不存在" rejection. [`plan_profile`] keeps
//! exactly that observable effect and nothing else.

use sea_orm::{ActiveModelTrait, Set};

use crate::data::members;
use crate::merchant::MembersRepo;
use crate::state::GatewayResult;

/// Rejection when `agentname` is supplied but names no existing member.
pub const MSG_AGENT_NOT_FOUND: &str = "代理商不存在";

/// The columns `saveProfile` may write (`allowField`, §10 L154), typed by their
/// member column. Each `Some(_)` means "the key was POSTed" — an empty string
/// is still a write of `""` (matching the legacy `save($p)`) — while `None`
/// leaves the column untouched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProfileUpdate {
    pub realname: Option<String>,
    pub sfznumber: Option<String>,
    pub mobile: Option<String>,
    pub qq: Option<String>,
    pub sex: Option<i32>,
    pub birthday: Option<i64>,
    pub address: Option<String>,
    pub login_ip: Option<String>,
    pub df_api: Option<i32>,
    pub df_auto_check: Option<i32>,
    pub df_domain: Option<String>,
    pub df_ip: Option<String>,
}

impl ProfileUpdate {
    /// True when nothing was whitelisted for write.
    pub fn is_empty(&self) -> bool {
        self == &ProfileUpdate::default()
    }
}

/// Turns a POSTed field map into the whitelisted [`ProfileUpdate`], applying
/// the §10 rules in order: only the 12 allowed columns are kept (everything
/// else — `id`, `username`, `parentid`, `agentname` … — is stripped); the int
/// columns parse leniently (empty / junk → `0`); `birthday` runs through a
/// `strtotime`-equivalent (unparseable → `0`); a non-empty `agentname` that
/// names no member rejects the whole save. `agent_exists` answers the member
/// lookup so the kernel stays DB-free.
pub fn plan_profile(
    posted: &[(&str, String)],
    agent_exists: impl Fn(&str) -> bool,
) -> Result<ProfileUpdate, &'static str> {
    let mut u = ProfileUpdate::default();
    for (key, value) in posted {
        match *key {
            "realname" => u.realname = Some(value.clone()),
            "sfznumber" => u.sfznumber = Some(value.clone()),
            "mobile" => u.mobile = Some(value.clone()),
            "qq" => u.qq = Some(value.clone()),
            "address" => u.address = Some(value.clone()),
            "login_ip" => u.login_ip = Some(value.clone()),
            "df_domain" => u.df_domain = Some(value.clone()),
            "df_ip" => u.df_ip = Some(value.clone()),
            "sex" => u.sex = Some(parse_int(value)),
            "df_api" => u.df_api = Some(parse_int(value)),
            "df_auto_check" => u.df_auto_check = Some(parse_int(value)),
            "birthday" => u.birthday = Some(parse_birthday(value)),
            "agentname" => {
                let name = value.trim();
                if !name.is_empty() && !agent_exists(name) {
                    return Err(MSG_AGENT_NOT_FOUND);
                }
                // Otherwise cosmetic: the reassignment is discarded below.
            }
            // Any other key is stripped by the whitelist filter.
            _ => {}
        }
    }
    Ok(u)
}

/// Persists a [`ProfileUpdate`] on the member, writing ONLY the present
/// columns. `Ok(false)` when the target member does not exist (the legacy
/// `where(id)->save` then touches 0 rows).
pub async fn apply_profile(
    db: &sea_orm::DatabaseConnection,
    user_id: i64,
    u: &ProfileUpdate,
) -> GatewayResult<bool> {
    let Some(model) = MembersRepo::new(db).by_id(user_id).await? else {
        return Ok(false);
    };
    if u.is_empty() {
        return Ok(true);
    }
    let mut active: members::ActiveModel = model.into();
    if let Some(v) = &u.realname {
        active.realname = Set(Some(v.clone()));
    }
    if let Some(v) = &u.sfznumber {
        active.sfznumber = Set(Some(v.clone()));
    }
    if let Some(v) = &u.mobile {
        active.mobile = Set(Some(v.clone()));
    }
    if let Some(v) = &u.qq {
        active.qq = Set(Some(v.clone()));
    }
    if let Some(v) = &u.address {
        active.address = Set(Some(v.clone()));
    }
    if let Some(v) = &u.login_ip {
        active.login_ip = Set(Some(v.clone()));
    }
    if let Some(v) = &u.df_domain {
        active.df_domain = Set(Some(v.clone()));
    }
    if let Some(v) = &u.df_ip {
        active.df_ip = Set(Some(v.clone()));
    }
    if let Some(v) = u.sex {
        active.sex = Set(Some(v));
    }
    if let Some(v) = u.birthday {
        active.birthday = Set(Some(v));
    }
    if let Some(v) = u.df_api {
        active.df_api = Set(v);
    }
    if let Some(v) = u.df_auto_check {
        active.df_auto_check = Set(v);
    }
    active.update(db).await?;
    Ok(true)
}

/// Legacy int-column write: `intval`-ish, empty / junk → `0`.
fn parse_int(value: &str) -> i32 {
    value.trim().parse::<i32>().unwrap_or(0)
}

/// `strtotime`-equivalent for `birthday`: `Y-m-d H:i:s` or `Y-m-d` (midnight)
/// in the local zone; empty / unparseable → `0`.
fn parse_birthday(value: &str) -> i64 {
    let s = value.trim();
    if s.is_empty() {
        return 0;
    }
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").or_else(|_| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap())
    });
    match naive {
        Ok(dt) => dt
            .and_local_timezone(chrono::Local)
            .single()
            .map_or(0, |z| z.timestamp()),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post<'a>(pairs: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, String)> {
        pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    #[test]
    fn whitelist_keeps_only_the_allowed_columns() {
        let posted = post(&[
            ("realname", "张三"),
            ("mobile", "13800000000"),
            ("id", "999"),
            ("username", "victim"),
            ("parentid", "7"),
            ("balance", "999999"),
        ]);
        let u = plan_profile(&posted, |_| true).unwrap();
        assert_eq!(u.realname.as_deref(), Some("张三"));
        assert_eq!(u.mobile.as_deref(), Some("13800000000"));
        // id / username / parentid / balance are stripped — there is no field
        // to even carry them.
        assert_eq!(u.qq, None);
        assert_eq!(u.address, None);
    }

    #[test]
    fn an_absent_agentname_writes_nothing_special() {
        // No agentname key at all → plain Ok (legacy else-branch parentid=1 is
        // discarded anyway).
        let posted = post(&[("realname", "李四")]);
        assert_eq!(
            plan_profile(&posted, |_| false)
                .unwrap()
                .realname
                .as_deref(),
            Some("李四")
        );
    }

    #[test]
    fn an_unknown_agentname_rejects_the_whole_save() {
        let posted = post(&[("realname", "王五"), ("agentname", "ghost")]);
        // agent_exists("ghost") → false.
        let err = plan_profile(&posted, |_| false).unwrap_err();
        assert_eq!(err, MSG_AGENT_NOT_FOUND);
        // A known agent clears the gate (but still writes no parentid).
        let u = plan_profile(&posted, |name| name == "ghost").unwrap();
        assert_eq!(u.realname.as_deref(), Some("王五"));
    }

    #[test]
    fn an_empty_agentname_is_not_validated() {
        let posted = post(&[("agentname", "  ")]);
        // A blank agent never reaches the lookup.
        assert!(plan_profile(&posted, |_| false).is_ok());
    }

    #[test]
    fn int_and_birthday_fields_parse() {
        let posted = post(&[
            ("sex", "2"),
            ("df_api", ""),
            ("df_auto_check", "junk"),
            ("birthday", "2000-01-02 03:04:05"),
        ]);
        let u = plan_profile(&posted, |_| true).unwrap();
        assert_eq!(u.sex, Some(2));
        assert_eq!(u.df_api, Some(0), "empty int → 0");
        assert_eq!(u.df_auto_check, Some(0), "junk int → 0");
        assert!(u.birthday.unwrap() > 0, "a real date parses to a ts");

        // A date-only string still parses; garbage → 0.
        let u2 = plan_profile(&post(&[("birthday", "2000-01-02")]), |_| true).unwrap();
        assert!(u2.birthday.unwrap() > 0);
        let u3 = plan_profile(&post(&[("birthday", "not-a-date")]), |_| true).unwrap();
        assert_eq!(u3.birthday, Some(0));
    }

    #[test]
    fn present_empty_string_is_a_write_absent_is_not() {
        let present = plan_profile(&post(&[("mobile", "")]), |_| true).unwrap();
        assert_eq!(present.mobile.as_deref(), Some(""));
        assert!(!present.is_empty());
        let absent = plan_profile(&post(&[]), |_| true).unwrap();
        assert!(absent.is_empty());
    }
}
