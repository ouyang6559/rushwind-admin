//! Merchant / agent registration — the offline-testable port of
//! `User/LoginController::checkRegister` (L226-290) and
//! `Common/Common/function.php::generateUser` (L787-835), `spec/05` §3.
//!
//! The rules (password match, email shape, username uniqueness, invite-code
//! validity) and the record assembly (salt, `md5(pwd.salt)`, default pay
//! password, `apikey`, `groupid`/`parentid`, `status`/`authorized` from the
//! site switches) are split from persistence: [`validate_local`] and
//! [`build_member_record`] are pure, while [`MembersRepo::find_by_username`]
//! / [`MembersRepo::create`] touch the database. The back-office
//! "add user" path (`Admin/UserController::saveUser`) and the agent
//! "open merchant" path (`User/AgentController::saveUser`) reuse the exact
//! same [`build_member_record`] with a different `groupid`/`parentid`.

use rand::Rng;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use crate::data::members;
use crate::merchant::password;
use crate::merchant::MembersRepo;
use crate::state::GatewayResult;

/// A registration rejection, keyed to the legacy `errorno` (`spec/05` §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// 10002 — `password != confirmpassword`.
    PasswordMismatch,
    /// 10001 — invite code required/invalid when the switch is on.
    InviteInvalid,
    /// 10005 — username already taken.
    UsernameTaken,
    /// `checkemail` — email fails `FILTER_VALIDATE_EMAIL`.
    EmailInvalid,
}

impl RegisterError {
    /// The legacy `errorno` (`checkRegister` §3.1). `EmailInvalid` has no
    /// `checkRegister` code (legacy only validated email in the separate
    /// `checkemail` AJAX), so it takes a fresh 10006.
    pub fn errorno(self) -> i64 {
        match self {
            RegisterError::InviteInvalid => 10001,
            RegisterError::PasswordMismatch => 10002,
            RegisterError::UsernameTaken => 10005,
            RegisterError::EmailInvalid => 10006,
        }
    }

    /// The caller-facing legacy message.
    pub fn message(self) -> &'static str {
        match self {
            RegisterError::InviteInvalid => "邀请码无效!",
            RegisterError::PasswordMismatch => "密码输入不一致!",
            RegisterError::UsernameTaken => "用户名重复!",
            RegisterError::EmailInvalid => "邮箱格式不正确!",
        }
    }
}

/// The raw registration form (`checkRegister` inputs).
#[derive(Debug, Clone, Copy, Default)]
pub struct RegisterInput<'a> {
    pub username: &'a str,
    pub password: &'a str,
    pub confirm_password: &'a str,
    pub email: &'a str,
    pub invite_code: &'a str,
}

/// The registration-relevant `pay_websiteconfig` switches (`spec/05` §2.3)
/// plus the site key the activation token is derived from. Carries a `String`
/// (the key) so it is `Clone` but not `Copy`; callers pass it by reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteFlags {
    /// `invitecode` — registration requires a valid invite code.
    pub invitecode: bool,
    /// `authorized` — merchants must complete KYC (seeds `authorized=0`).
    pub authorized: bool,
    /// `register_need_activate` — email activation required (seeds `status=0`).
    pub register_need_activate: bool,
    /// `DATA_AUTH_KEY` — the server secret mixed into the activation token
    /// (`generateUser` L796). Empty is legal (dev) but the token is then
    /// weak; production wires the config value through [`crate::config`].
    pub data_auth_key: String,
}

/// A fully assembled `members` insert row for a new merchant/agent
/// (money-unit balances start at zero).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMember {
    pub username: String,
    /// The `md5(password . salt)` login hash.
    pub password: String,
    pub groupid: i32,
    pub salt: String,
    pub parentid: i64,
    pub email: Option<String>,
    /// The MD5 sign secret (`random_str()`), returned to the caller once.
    pub apikey: String,
    /// The default payment-password hash (`md5('123456')`).
    pub pay_password: String,
    pub status: i32,
    pub authorized: i32,
    /// The email-activation link token (`generateUser` L796), always seeded;
    /// only consumed when `register_need_activate` gates the account.
    pub activate: String,
}

/// Validates everything the register path can decide without the database or
/// site switches (`checkRegister` L235-237, `checkemail` L311-326).
pub fn validate_local(input: &RegisterInput<'_>) -> Result<(), RegisterError> {
    if input.password != input.confirm_password {
        return Err(RegisterError::PasswordMismatch);
    }
    if !is_valid_email(input.email) {
        return Err(RegisterError::EmailInvalid);
    }
    Ok(())
}

/// A faithful-enough `FILTER_VALIDATE_EMAIL`: one `@`, non-empty local part,
/// a dot-separated domain with at least two labels and no spaces. Rejects the
/// common malformed cases without chasing the full RFC grammar.
pub fn is_valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || email.chars().any(char::is_whitespace) {
        return false;
    }
    // domain: at least two labels, each non-empty, ASCII-alnum/hyphen.
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// The activation-link token (`generateUser` L796):
/// `md5( md5(username) · md5(plaintext_password) · md5(email) · DATA_AUTH_KEY )`.
/// Faithful to the PHP concat — an empty email still hashes to
/// `md5("")`. The `password` argument is the SAME plaintext that feeds the
/// login hash (not the salted hash), so the token is reproducible only with
/// the original form password + the site key.
pub fn build_activate_code(
    username: &str,
    password_plaintext: &str,
    email: &str,
    data_auth_key: &str,
) -> String {
    let md5 = |s: &str| format!("{:x}", md5::compute(s.as_bytes()));
    let joined = format!(
        "{}{}{}{}",
        md5(username),
        md5(password_plaintext),
        md5(email),
        data_auth_key
    );
    md5(&joined)
}

/// The `generateUser` record assembly (`spec/05` §3.3): derives `status` and
/// `authorized` from the site switches, seeds the default pay password and a
/// fresh API key, and fixes the groupid/parentid. The `password` argument is
/// the already-decided plaintext (registration form, or the 6-char random for
/// a back-office/agent add when none is supplied).
pub fn build_member_record(
    username: &str,
    password_plaintext: &str,
    email: &str,
    groupid: i32,
    parentid: i64,
    flags: &SiteFlags,
) -> NewMember {
    let salt = password::new_salt();
    NewMember {
        username: username.to_string(),
        password: password::hash_password(password_plaintext, &salt),
        groupid,
        parentid,
        salt: salt.clone(),
        email: (!email.is_empty()).then(|| email.to_string()),
        apikey: crate::merchant::generate_apikey(),
        pay_password: password::hash_pay_password(password::DEFAULT_PAY_PASSWORD),
        status: if flags.register_need_activate { 0 } else { 1 },
        authorized: if flags.authorized { 0 } else { 1 },
        activate: build_activate_code(username, password_plaintext, email, &flags.data_auth_key),
    }
}

/// Runs the whole §3.1 `checkRegister` write path against the database, in the
/// legacy order: local checks (password / email) → the invite gate (10001,
/// only when `flags.invitecode`) → username uniqueness (10005) → the member
/// insert → invite consumption (`status = 2 / syusernameid / sydatetime`). The
/// resolved invite fixes the new member's `groupid` (`regtype` else 4) and
/// `parentid` (the minting agent, else the platform 1); with the switch off, a
/// plain merchant is created under the platform. The outer `Err` is an
/// infrastructure failure, the inner `Err` the domain `errorno` reason.
pub async fn register_member(
    db: &sea_orm::DatabaseConnection,
    input: &RegisterInput<'_>,
    flags: &SiteFlags,
) -> crate::state::GatewayResult<Result<i64, RegisterError>> {
    if let Err(e) = validate_local(input) {
        return Ok(Err(e));
    }
    let now = crate::data::now_ts();

    // Invite gate (§3.1 L240-246) — only when the site requires one.
    let invite = if flags.invitecode {
        match crate::merchant::invite::find_usable(db, input.invite_code, now).await? {
            Some(i) => Some(i),
            None => return Ok(Err(RegisterError::InviteInvalid)),
        }
    } else {
        None
    };

    // Username uniqueness (§3.1 L248-251), checked AFTER the invite gate.
    let repo = MembersRepo::new(db);
    if repo.by_username(input.username).await?.is_some() {
        return Ok(Err(RegisterError::UsernameTaken));
    }

    let (groupid, parentid) = match &invite {
        Some(i) => crate::merchant::invite::apply_invite_to_member(i),
        None => (4, 1),
    };
    let record = build_member_record(
        input.username,
        input.password,
        input.email,
        groupid,
        parentid,
        flags,
    );
    let new_uid = repo.create(&record).await?;

    // Invalidate the consumed code (§3.1 L271-272), matched by its value.
    if let Some(i) = &invite {
        crate::merchant::invite::consume_invite(db, &i.invitecode, new_uid, now).await?;
    }
    Ok(Ok(new_uid))
}

impl<'a> MembersRepo<'a> {
    /// Persists a freshly assembled [`NewMember`] and returns its id
    /// (the wire merchant number is `id + 10000`).
    pub async fn create(&self, row: &NewMember) -> Result<i64, sea_orm::DbErr> {
        let active = members::ActiveModel {
            username: Set(row.username.clone()),
            password: Set(row.password.clone()),
            groupid: Set(row.groupid),
            salt: Set(row.salt.clone()),
            parentid: Set(row.parentid),
            balance: Set(0),
            blocked_balance: Set(0),
            email: Set(row.email.clone()),
            apikey: Set(Some(row.apikey.clone())),
            pay_password: Set(Some(row.pay_password.clone())),
            status: Set(row.status),
            authorized: Set(row.authorized),
            activate: Set(Some(row.activate.clone())),
            df_api: Set(0),
            ..Default::default()
        };
        let inserted = active.insert(self.db).await?;
        Ok(inserted.id)
    }
}

/// The outcome of an activation-link visit (`Home/EmptyController::_empty`, the
/// `Activate` branch, `spec/05` §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateOutcome {
    /// No member carries the token — the legacy `账号有误，激活失败！`.
    InvalidToken,
    /// The account is already `status != 0` — `您已激活！` (idempotent, no write).
    AlreadyActive,
    /// The pending account was flipped to `status = 1` — `激活成功!`.
    Activated,
}

/// Runs the §3.4 activation link against the database: finds the member by the
/// exact `activate` token and, if still pending (`status = 0`), enables it.
/// A non-pending match short-circuits without a write (idempotent re-clicks).
/// The `activatedatetime` display column the legacy also stamps is unmodeled
/// (a display-only field like `regdatetime`, read by nothing).
pub async fn activate_member(
    db: &sea_orm::DatabaseConnection,
    token: &str,
) -> GatewayResult<ActivateOutcome> {
    if token.is_empty() {
        return Ok(ActivateOutcome::InvalidToken);
    }
    let member = members::Entity::find()
        .filter(members::Column::Activate.eq(token))
        .one(db)
        .await?;
    Ok(match member {
        None => ActivateOutcome::InvalidToken,
        Some(m) if m.status != 0 => ActivateOutcome::AlreadyActive,
        Some(m) => {
            let mut active: members::ActiveModel = m.into();
            active.status = Set(1);
            active.update(db).await?;
            ActivateOutcome::Activated
        }
    })
}

/// A §6.1 agent-open duplicate rejection (`User/AgentController::saveUser`
/// L457-465).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// A member already owns the username.
    UsernameTaken,
    /// A member already owns the email.
    EmailTaken,
}

impl OpenError {
    /// The exact legacy `ajaxReturn` message.
    pub fn message(self) -> &'static str {
        match self {
            OpenError::UsernameTaken => "用户名已存在",
            OpenError::EmailTaken => "邮箱已存在",
        }
    }
}

/// A fresh `[a-z0-9]` random string of `len`, the legacy `random_str()`
/// alphabet (`Common/Common/function.php:61-69`).
pub fn random_str(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// The §6.1 "open a downline merchant" form: the fields the agent form posts
/// that reach `generateUser` (`birthday` is display-only and unmodeled, like
/// `activatedatetime`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenChildInput<'a> {
    pub username: &'a str,
    pub email: &'a str,
    /// Empty → a fresh `random_str(6)` that the (Noop) email would deliver.
    pub password: &'a str,
}

/// Runs the §6.1 `saveUser` write: a `username`-then-`email` duplicate check,
/// then a [`build_member_record`] ALWAYS at `groupid = 4` parented to the
/// acting agent (the legacy `saveUser` never overrides the child groupid,
/// L472). Faithful to that path it applies NO tier / balance / quota gate (the
/// §6.1 缺陷) and inherits the site switches only through `flags` (`status` /
/// `authorized`). `sendPasswordEmail` (L478) is a Noop seam. Returns the new
/// member id, or the duplicate reason.
///
/// Deviation: the legacy runs a single `where(username OR email)->find()` and
/// reports whichever row has the smaller id; here the username check is
/// deterministic-first (the sane precedence), so a username clash always wins
/// over a co-existing email clash.
pub async fn open_downline_merchant(
    db: &sea_orm::DatabaseConnection,
    agent_uid: i64,
    input: &OpenChildInput<'_>,
    flags: &SiteFlags,
) -> GatewayResult<Result<i64, OpenError>> {
    if !members::Entity::find()
        .filter(members::Column::Username.eq(input.username))
        .all(db)
        .await?
        .is_empty()
    {
        return Ok(Err(OpenError::UsernameTaken));
    }
    if !members::Entity::find()
        .filter(members::Column::Email.eq(input.email))
        .all(db)
        .await?
        .is_empty()
    {
        return Ok(Err(OpenError::EmailTaken));
    }
    let password_plaintext = if input.password.is_empty() {
        random_str(6)
    } else {
        input.password.to_string()
    };
    let record = build_member_record(
        input.username,
        &password_plaintext,
        input.email,
        4,
        agent_uid,
        flags,
    );
    let new_uid = MembersRepo::new(db).create(&record).await?;
    Ok(Ok(new_uid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(pw: &'a str, confirm: &'a str, email: &'a str) -> RegisterInput<'a> {
        RegisterInput {
            username: "mch01",
            password: pw,
            confirm_password: confirm,
            email,
            invite_code: "",
        }
    }

    #[test]
    fn validate_rejects_mismatch_and_bad_email() {
        assert_eq!(
            validate_local(&input("a1", "a2", "ok@x.com")),
            Err(RegisterError::PasswordMismatch)
        );
        assert_eq!(
            validate_local(&input("a1", "a1", "not-an-email")),
            Err(RegisterError::EmailInvalid)
        );
        assert_eq!(validate_local(&input("a1", "a1", "ok@x.com")), Ok(()));
    }

    #[test]
    fn email_shape() {
        assert!(is_valid_email("a@b.co"));
        assert!(is_valid_email("first.last@sub.example.com"));
        assert!(!is_valid_email("a@b")); // single-label domain
        assert!(!is_valid_email("@b.co")); // empty local
        assert!(!is_valid_email("a@.co")); // empty label
        assert!(!is_valid_email("a b@c.co")); // whitespace
        assert!(!is_valid_email("a@b_c.co")); // underscore in domain label
    }

    #[test]
    fn record_defaults_to_enabled_merchant_with_seeded_secrets() {
        let flags = SiteFlags::default();
        let rec = build_member_record("mch01", "pw123", "a@b.co", 4, 1, &flags);
        assert_eq!(rec.groupid, 4);
        assert_eq!(rec.parentid, 1);
        assert_eq!(rec.status, 1, "activate off → enabled");
        assert_eq!(rec.authorized, 1, "auth not required → authorized");
        assert_eq!(rec.salt.len(), 4);
        assert_eq!(rec.password, password::hash_password("pw123", &rec.salt));
        assert_eq!(rec.pay_password, password::hash_pay_password("123456"));
        assert_eq!(rec.apikey.len(), 32);
        assert!(rec.apikey.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn site_flags_gate_status_and_authorized() {
        let flags = SiteFlags {
            invitecode: true,
            authorized: true,
            register_need_activate: true,
            ..Default::default()
        };
        let rec = build_member_record("mch02", "pw", "", 4, 7, &flags);
        assert_eq!(rec.status, 0, "activation required → disabled until email");
        assert_eq!(rec.authorized, 0, "KYC required → not authorized");
        assert_eq!(rec.email, None, "empty email stores NULL");
        assert_eq!(rec.parentid, 7, "invite owner becomes the parent");
    }

    #[test]
    fn activate_code_is_the_legacy_md5_chain() {
        // md5( md5(user)·md5(pw)·md5(email)·KEY ), reproduced byte-for-byte.
        let md5 = |s: &str| format!("{:x}", md5::compute(s.as_bytes()));
        let want = md5(&format!(
            "{}{}{}{}",
            md5("mch01"),
            md5("pw123"),
            md5("a@b.co"),
            "KEY"
        ));
        assert_eq!(build_activate_code("mch01", "pw123", "a@b.co", "KEY"), want);
        // a different key changes the token (it is the unguessable secret).
        assert_ne!(
            build_activate_code("mch01", "pw123", "a@b.co", "KEY"),
            build_activate_code("mch01", "pw123", "a@b.co", "OTHER")
        );
    }

    #[test]
    fn record_always_seeds_an_activate_token() {
        let flags = SiteFlags::default();
        let rec = build_member_record("mch01", "pw123", "a@b.co", 4, 1, &flags);
        assert_eq!(rec.activate.len(), 32, "md5 hex token");
        assert!(rec.activate.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            rec.activate,
            build_activate_code("mch01", "pw123", "a@b.co", &flags.data_auth_key)
        );
    }

    #[test]
    fn register_errors_carry_the_legacy_errorno_and_message() {
        assert_eq!(RegisterError::PasswordMismatch.errorno(), 10002);
        assert_eq!(RegisterError::InviteInvalid.errorno(), 10001);
        assert_eq!(RegisterError::UsernameTaken.errorno(), 10005);
        assert_eq!(RegisterError::EmailInvalid.errorno(), 10006);
        assert_eq!(RegisterError::PasswordMismatch.message(), "密码输入不一致!");
        assert_eq!(RegisterError::InviteInvalid.message(), "邀请码无效!");
        assert_eq!(RegisterError::UsernameTaken.message(), "用户名重复!");
        assert_eq!(RegisterError::EmailInvalid.message(), "邮箱格式不正确!");
    }

    #[test]
    fn random_str_matches_the_legacy_alphabet() {
        let s = random_str(6);
        assert_eq!(s.len(), 6);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        // two draws differ (overwhelmingly) for a 36⁶ space.
        assert_ne!(random_str(32), random_str(32));
    }

    #[test]
    fn open_errors_carry_the_legacy_messages() {
        assert_eq!(OpenError::UsernameTaken.message(), "用户名已存在");
        assert_eq!(OpenError::EmailTaken.message(), "邮箱已存在");
    }
}
