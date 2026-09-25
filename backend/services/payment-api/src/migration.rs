//! Schema bootstrap — the framework's migration facility
//! (`rushwind-storage-seaorm-migration`) driven by the entity catalog in
//! [`crate::data`], applied once and tracked, at startup when the
//! `database.migrate` gate is on. A second migration adds the idempotency
//! and scan indexes the entity catalog cannot express (the `EntityTables`
//! builder only derives `CREATE TABLE`; indexes ride a custom migration,
//! per the crate docs).

use rushwind_storage_seaorm_migration::{
    EntityTables, MigrationName, MigrationTrait, MigratorTrait, SchemaManager,
};
use sea_orm::sea_query::{Alias, Index};
use sea_orm::{ConnectOptions, ConnectionTrait, DatabaseBackend, DbErr};

use crate::data;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        let init = EntityTables::new("m20260919_000001_payment_init", DatabaseBackend::Postgres)
            .table::<data::members::Entity>()
            .table::<data::orders::Entity>()
            .table::<data::money_changes::Entity>()
            .table::<data::channels::Entity>()
            .table::<data::channel_accounts::Entity>()
            .table::<data::products::Entity>()
            .table::<data::product_users::Entity>()
            .table::<data::user_channel_accounts::Entity>()
            .table::<data::user_rates::Entity>()
            .table::<data::blocked_logs::Entity>()
            .table::<data::tikuan_configs::Entity>()
            .table::<data::tikuan_holidays::Entity>()
            .build();
        // The §8 statement table ships as its own migration: databases
        // that already tracked `init` must still pick the table up.
        let recon = EntityTables::new(
            "m20260921_000003_reconciliations",
            DatabaseBackend::Postgres,
        )
        .table::<data::reconciliations::Entity>()
        .build();
        vec![
            Box::new(init),
            Box::new(Indexes),
            Box::new(recon),
            Box::new(ReconIndex),
            Box::new(RiskColumns),
            Box::new(UserRiskConfigs),
            Box::new(ScreenColumns),
            Box::new(PayoutOrders),
            Box::new(PayoutExecColumns),
            Box::new(PayoutChannels),
            Box::new(TikuanAutoDfColumns),
            Box::new(MemberDfColumns),
            Box::new(InviteCodes),
            Box::new(MemberLoginSessionColumns),
            Box::new(MemberProfileColumns),
            Box::new(Bankcards),
            Box::new(Attachments),
            Box::new(Loginrecords),
            Box::new(MemberReceiverColumn),
            Box::new(MemberActivateColumn),
            Box::new(Articles),
            Box::new(ComplaintsDeposits),
            Box::new(ComplaintsDepositRules),
            Box::new(NotifyLogs),
            Box::new(RedoOrders),
            Box::new(SmsConfigs),
            Box::new(SmsTemplates),
            Box::new(UserCodes),
        ]
    }
}

/// The indexes the entity catalog cannot carry: the order idempotency key,
/// the fund-flow dedup key, and the reissue/thaw scan access paths
/// (`spec/02-funds-order.md` §11.4).
struct Indexes;

/// The `up` body runs once (tracked); `IF NOT EXISTS` keeps a partial
/// first run idempotent across the index set.
impl MigrationName for Indexes {
    fn name(&self) -> &str {
        "m20260919_000002_payment_indexes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Indexes {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        create_unique(manager, "uq_orders_order_id", "orders", "order_id").await?;
        create_unique(
            manager,
            "uq_money_changes_request_id",
            "money_changes",
            "request_id",
        )
        .await?;
        create_index(
            manager,
            "idx_orders_status_reissue",
            "orders",
            &["status", "num", "last_reissue_time"],
        )
        .await?;
        create_index(
            manager,
            "idx_blocked_logs_status_thaw",
            "blocked_logs",
            &["status", "thaw_time"],
        )
        .await?;
        create_index(
            manager,
            "idx_product_users_user",
            "product_users",
            &["user_id"],
        )
        .await?;
        create_index(
            manager,
            "idx_user_channel_accounts_user",
            "user_channel_accounts",
            &["user_id"],
        )
        .await?;
        Ok(())
    }
}

async fn create_unique(
    mgr: &SchemaManager<'_>,
    name: &str,
    table: &str,
    col: &str,
) -> Result<(), DbErr> {
    let mut idx = Index::create();
    idx.name(name)
        .table(Alias::new(table))
        .col(Alias::new(col))
        .unique();
    mgr.create_index(idx).await
}

/// The merchant×day uniqueness the legacy left to find-then-add racing
/// (`spec/02` §8) — here it backs the statement's UPSERT, so two lazy
/// readers can never fork a day's snapshot into twin rows.
struct ReconIndex;

impl MigrationName for ReconIndex {
    fn name(&self) -> &str {
        "m20260921_000004_reconciliation_index"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ReconIndex {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let mut idx = Index::create();
        idx.name("uq_reconciliations_user_date")
            .table(Alias::new("reconciliations"))
            .col(Alias::new("user_id"))
            .col(Alias::new("date"))
            .unique();
        manager.create_index(idx).await
    }
}

/// The `spec/06` §2.1/§2.2 risk-config columns the post-settle counters
/// (`risk::observe`) read. Databases that already tracked `init` pick them
/// up here; `IF NOT EXISTS` keeps fresh `init`-created tables (where the
/// entity catalog already carries the columns) a no-op.
struct RiskColumns;

impl MigrationName for RiskColumns {
    fn name(&self) -> &str {
        "m20260921_000005_risk_columns"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for RiskColumns {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Defaults mirror the legacy DDL (sifang.sql:765-772): a plain
        // account inherits its channel (`is_defined` 0), the unit bucket
        // counts seconds, caps of 0 mean "unlimited".
        let stmts = [
            "ALTER TABLE channels ADD COLUMN IF NOT EXISTS all_money bigint NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS is_defined integer NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS all_money bigint NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS unit_interval integer NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS time_unit varchar(1) NOT NULL DEFAULT 's'",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS unit_number bigint NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS unit_all_money bigint NOT NULL DEFAULT 0",
            // On a fresh `init` the entity catalog already created these
            // non-`Option` columns WITHOUT a default, so the `IF NOT EXISTS`
            // above no-ops and the default is lost — breaking every
            // `..Default::default()` insert. Re-pin the defaults (idempotent
            // for backfilled databases) so fresh and legacy schemas agree.
            "ALTER TABLE channels ALTER COLUMN all_money SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN is_defined SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN all_money SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN unit_interval SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN time_unit SET DEFAULT 's'",
            "ALTER TABLE channel_accounts ALTER COLUMN unit_number SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN unit_all_money SET DEFAULT 0",
        ];
        for stmt in stmts {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        Ok(())
    }
}

/// The merchant-screening rule table (`spec/06` §2.3). Its own migration so
/// databases that already tracked `init` pick the table up too.
struct UserRiskConfigs;

impl MigrationName for UserRiskConfigs {
    fn name(&self) -> &str {
        "m20260921_000006_user_risk_configs"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for UserRiskConfigs {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new(
            "m20260921_000006_user_risk_configs",
            DatabaseBackend::Postgres,
        )
        .table::<data::user_riskcontrol_configs::Entity>()
        .build();
        create.up(manager).await?;
        // findConfigInfo serves the platform fallback row by (is_system,
        // status) and the merchant row by the unique user_id — the legacy
        // left the lookup paths unindexed.
        create_index(
            manager,
            "idx_user_risk_configs_system",
            "user_riskcontrol_configs",
            &["is_system"],
        )
        .await
    }
}

/// The order-side screening columns (`spec/06` §2.1/§2.2): trading-window
/// hours and per-transaction bounds on the channel and account rows. Same
/// dual-state idempotence as [`RiskColumns`]: `IF NOT EXISTS` no-ops on
/// fresh `init`-created tables, backfills the tracked legacy ones.
struct ScreenColumns;

impl MigrationName for ScreenColumns {
    fn name(&self) -> &str {
        "m20260921_000007_screen_columns"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ScreenColumns {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmts = [
            "ALTER TABLE channels ADD COLUMN IF NOT EXISTS start_time integer NOT NULL DEFAULT 0",
            "ALTER TABLE channels ADD COLUMN IF NOT EXISTS end_time integer NOT NULL DEFAULT 0",
            "ALTER TABLE channels ADD COLUMN IF NOT EXISTS min_money bigint NOT NULL DEFAULT 0",
            "ALTER TABLE channels ADD COLUMN IF NOT EXISTS max_money bigint NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS start_time integer NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS end_time integer NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS min_money bigint NOT NULL DEFAULT 0",
            "ALTER TABLE channel_accounts ADD COLUMN IF NOT EXISTS max_money bigint NOT NULL DEFAULT 0",
            // Fresh `init` shapes these non-`Option` columns without a
            // default, so re-pin `DEFAULT 0` (idempotent for backfilled DBs).
            "ALTER TABLE channels ALTER COLUMN start_time SET DEFAULT 0",
            "ALTER TABLE channels ALTER COLUMN end_time SET DEFAULT 0",
            "ALTER TABLE channels ALTER COLUMN min_money SET DEFAULT 0",
            "ALTER TABLE channels ALTER COLUMN max_money SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN start_time SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN end_time SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN min_money SET DEFAULT 0",
            "ALTER TABLE channel_accounts ALTER COLUMN max_money SET DEFAULT 0",
        ];
        for stmt in stmts {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        Ok(())
    }
}

/// The unified payout order table (`spec/04` §13.1) and its uniqueness
/// keys. Its own migration so tracked databases pick the table up too.
struct PayoutOrders;

impl MigrationName for PayoutOrders {
    fn name(&self) -> &str {
        "m20260922_000008_payout_orders"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for PayoutOrders {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260922_000008_payout_orders", DatabaseBackend::Postgres)
            .table::<data::payout_orders::Entity>()
            .build();
        create.up(manager).await?;
        // Platform order no is unique (§13.4).
        create_unique(
            manager,
            "uq_payout_orders_order_no",
            "payout_orders",
            "order_no",
        )
        .await?;
        // The merchant×out-trade-no idempotency net replacing the legacy's
        // find-then-add race (§6.1/§12.6). Partial: own withdrawals keep
        // NULL out_trade_no and must not collide with each other.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX IF NOT EXISTS uq_payout_orders_out_trade \
                 ON payout_orders (user_id, out_trade_no) \
                 WHERE out_trade_no IS NOT NULL",
            )
            .await?;
        // Daily-rollup and listing access paths (§13.1 index note).
        create_index(
            manager,
            "idx_payout_orders_user_created",
            "payout_orders",
            &["user_id", "created_at"],
        )
        .await
    }
}

/// The execution-queue columns (`spec/04` §8/§10) the channel runner claims
/// and folds against: `df_lock` (atomic claim), the retry / query counters,
/// the auto flag, and the channel cost snapshot. Same dual-state idempotence
/// as [`RiskColumns`] — `IF NOT EXISTS` no-ops a fresh table the entity
/// catalog already shaped, backfills one tracked before this slice.
struct PayoutExecColumns;

impl MigrationName for PayoutExecColumns {
    fn name(&self) -> &str {
        "m20260922_000009_payout_exec_columns"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for PayoutExecColumns {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmts = [
            "ALTER TABLE payout_orders ADD COLUMN IF NOT EXISTS channel_mch_id varchar(64)",
            "ALTER TABLE payout_orders ADD COLUMN IF NOT EXISTS cost bigint NOT NULL DEFAULT 0",
            "ALTER TABLE payout_orders ADD COLUMN IF NOT EXISTS cost_rate bigint NOT NULL DEFAULT 0",
            "ALTER TABLE payout_orders ADD COLUMN IF NOT EXISTS rate_type integer NOT NULL DEFAULT 0",
            "ALTER TABLE payout_orders ADD COLUMN IF NOT EXISTS auto_submit_try integer NOT NULL DEFAULT 0",
            "ALTER TABLE payout_orders ADD COLUMN IF NOT EXISTS auto_query_num integer NOT NULL DEFAULT 0",
            "ALTER TABLE payout_orders ADD COLUMN IF NOT EXISTS is_auto integer NOT NULL DEFAULT 0",
            // Fresh `init` shapes these non-`Option` columns without a
            // default, so re-pin `DEFAULT 0` (idempotent for backfilled DBs).
            "ALTER TABLE payout_orders ALTER COLUMN cost SET DEFAULT 0",
            "ALTER TABLE payout_orders ALTER COLUMN cost_rate SET DEFAULT 0",
            "ALTER TABLE payout_orders ALTER COLUMN rate_type SET DEFAULT 0",
            "ALTER TABLE payout_orders ALTER COLUMN auto_submit_try SET DEFAULT 0",
            "ALTER TABLE payout_orders ALTER COLUMN auto_query_num SET DEFAULT 0",
            "ALTER TABLE payout_orders ALTER COLUMN is_auto SET DEFAULT 0",
        ];
        for stmt in stmts {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        // The submit-queue sweep reads status=0 / df_lock=0 rows in id order
        // (§8.1/§10.1); the query sweep reads status=1 by auto_query_num
        // (§10.2). Both were full scans on the legacy wttklist.
        create_index(
            manager,
            "idx_payout_orders_submit",
            "payout_orders",
            &["status", "df_lock", "id"],
        )
        .await?;
        create_index(
            manager,
            "idx_payout_orders_query",
            "payout_orders",
            &["status", "auto_query_num"],
        )
        .await
    }
}

/// The payout-channel (代付通道) configuration table (`pay_pay_for_another`,
/// `spec/04` §8/§10) the execution queue now reads its channel config from.
/// Its own migration so tracked databases pick the table up too. The legacy
/// keyed on `code`; the submit-path default / enable lookups ride
/// `(status, is_default)`, so both access paths get an index.
struct PayoutChannels;

impl MigrationName for PayoutChannels {
    fn name(&self) -> &str {
        "m20260922_000010_payout_channels"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for PayoutChannels {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new(
            "m20260922_000010_payout_channels",
            DatabaseBackend::Postgres,
        )
        .table::<data::payout_channels::Entity>()
        .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_payout_channels_code",
            "payout_channels",
            &["code"],
        )
        .await?;
        create_index(
            manager,
            "idx_payout_channels_default",
            "payout_channels",
            &["status", "is_default"],
        )
        .await
    }
}

/// The 自动代付 (`spec/04` §10.1) columns on `tikuan_configs`: the master
/// switch, the daily run window (`HH:MM` strings), the per-order ceiling and
/// the per-merchant daily count/amount caps. Same dual-state idempotence as
/// [`RiskColumns`] — `IF NOT EXISTS` no-ops a fresh `init`-created table (the
/// entity catalog now shapes these) and backfills one tracked before this
/// slice. Defaults are all off/unlimited (switch `0`, empty window, caps `0`),
/// so an unmigrated operator config keeps the auto sweep a no-op.
struct TikuanAutoDfColumns;

impl MigrationName for TikuanAutoDfColumns {
    fn name(&self) -> &str {
        "m20260922_000011_tikuan_auto_df_columns"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for TikuanAutoDfColumns {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmts = [
            "ALTER TABLE tikuan_configs ADD COLUMN IF NOT EXISTS auto_df_switch integer NOT NULL DEFAULT 0",
            "ALTER TABLE tikuan_configs ADD COLUMN IF NOT EXISTS auto_df_maxmoney bigint NOT NULL DEFAULT 0",
            "ALTER TABLE tikuan_configs ADD COLUMN IF NOT EXISTS auto_df_stime varchar(20) NOT NULL DEFAULT ''",
            "ALTER TABLE tikuan_configs ADD COLUMN IF NOT EXISTS auto_df_etime varchar(20) NOT NULL DEFAULT ''",
            "ALTER TABLE tikuan_configs ADD COLUMN IF NOT EXISTS auto_df_max_count bigint NOT NULL DEFAULT 0",
            "ALTER TABLE tikuan_configs ADD COLUMN IF NOT EXISTS auto_df_max_sum bigint NOT NULL DEFAULT 0",
            // Fresh `init` shapes the non-`Option` ones without a default, so
            // re-pin them (idempotent for backfilled DBs) to keep the auto
            // sweep a no-op for an unmigrated operator config.
            "ALTER TABLE tikuan_configs ALTER COLUMN auto_df_switch SET DEFAULT 0",
            "ALTER TABLE tikuan_configs ALTER COLUMN auto_df_maxmoney SET DEFAULT 0",
            "ALTER TABLE tikuan_configs ALTER COLUMN auto_df_stime SET DEFAULT ''",
            "ALTER TABLE tikuan_configs ALTER COLUMN auto_df_etime SET DEFAULT ''",
            "ALTER TABLE tikuan_configs ALTER COLUMN auto_df_max_count SET DEFAULT 0",
            "ALTER TABLE tikuan_configs ALTER COLUMN auto_df_max_sum SET DEFAULT 0",
        ];
        for stmt in stmts {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        Ok(())
    }
}

/// The payout-API (`spec/04` §7.1) reporting / auto-review columns on
/// `members`, off the legacy `pay_member` df_* fields the `Dfpay::add` wire
/// handler reads: the报备 base-domain whitelist, the client-IP whitelist and
/// the auto-review flag. Same dual-state idempotence as [`TikuanAutoDfColumns`]
/// — `IF NOT EXISTS` no-ops a fresh `init`-created table (the entity catalog
/// now shapes these) and backfills one tracked before this slice. The domain /
/// IP columns stay nullable (legacy empty = check disabled); `df_auto_check`
/// defaults `0` so an unmigrated merchant never auto-debits on filing.
struct MemberDfColumns;

impl MigrationName for MemberDfColumns {
    fn name(&self) -> &str {
        "m20260922_000012_member_df_columns"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for MemberDfColumns {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmts = [
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS df_domain varchar(500)",
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS df_ip varchar(500)",
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS df_auto_check integer NOT NULL DEFAULT 0",
            // On a fresh `init` the entity catalog already built
            // `df_auto_check` as `NOT NULL` WITHOUT a default, so the
            // `IF NOT EXISTS` above no-ops and the default is lost — which
            // breaks any `..Default::default()` seed that omits the column.
            // Pin `DEFAULT 0` on the (possibly pre-existing) column so fresh
            // and backfilled databases agree with the stated intent.
            "ALTER TABLE members ALTER COLUMN df_auto_check SET DEFAULT 0",
        ];
        for stmt in stmts {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        Ok(())
    }
}

/// The agent invite-code table (`pay_invitecode`, `spec/05` §6.3): the
/// lifecycle `status` gate the register path consumes plus the parallel
/// `inviteconfigzt` display flag, both modelled faithfully. Its own migration
/// so tracked databases pick the table up too. `invitecode` carries the legacy
/// UNIQUE key; the agent's own list reads by `fmusernameid`.
struct InviteCodes;

impl MigrationName for InviteCodes {
    fn name(&self) -> &str {
        "m20260922_000013_invite_codes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for InviteCodes {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260922_000013_invite_codes", DatabaseBackend::Postgres)
            .table::<data::invite_codes::Entity>()
            .build();
        create.up(manager).await?;
        create_unique(
            manager,
            "uq_invite_codes_code",
            "invite_codes",
            "invitecode",
        )
        .await?;
        create_index(
            manager,
            "idx_invite_codes_fmuser",
            "invite_codes",
            &["fmusernameid"],
        )
        .await
    }
}

/// The merchant login-IP whitelist and single-sign-on version columns
/// (`pay_member.login_ip` / `pay_member.session_random`, `spec/05` §4.2 / §4.6).
/// Both are nullable: an absent `login_ip` admits every client IP, and an
/// absent `session_version` simply means no panel login has minted one yet.
struct MemberLoginSessionColumns;

impl MigrationName for MemberLoginSessionColumns {
    fn name(&self) -> &str {
        "m20260922_000014_member_login_session_columns"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for MemberLoginSessionColumns {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmts = [
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS login_ip varchar(1000)",
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS session_version varchar(50)",
        ];
        for stmt in stmts {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        Ok(())
    }
}

/// The merchant profile columns `saveProfile` may write (`pay_member`
/// `sex/birthday/sfznumber/qq/address`, `spec/05` §10). All nullable: an
/// unfilled member reads `None`, so no existing seed / register path breaks.
struct MemberProfileColumns;

impl MigrationName for MemberProfileColumns {
    fn name(&self) -> &str {
        "m20260922_000015_member_profile_columns"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for MemberProfileColumns {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmts = [
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS sex integer",
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS birthday bigint",
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS sfznumber varchar(20)",
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS qq varchar(15)",
            "ALTER TABLE members ADD COLUMN IF NOT EXISTS address varchar(200)",
        ];
        for stmt in stmts {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        Ok(())
    }
}

/// The台卡 payee line column `saveReceiver` writes (`pay_member.receiver`,
/// `varchar(255)`, `spec/05` §10). Nullable, so an untouched member reads
/// `None` and no existing seed / register path breaks.
struct MemberReceiverColumn;

impl MigrationName for MemberReceiverColumn {
    fn name(&self) -> &str {
        "m20260924_000003_member_receiver"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for MemberReceiverColumn {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE members ADD COLUMN IF NOT EXISTS receiver varchar(255)",
            )
            .await?;
        Ok(())
    }
}

/// The email-activation link token (`members.activate`, `spec/05` §3.4): the
/// `generateUser` md5 code written at registration and read back by the
/// `Activate` link to flip a pending (`status = 0`) merchant to enabled. Its
/// own `ALTER` so tracked databases pick it up; nullable (a pre-existing
/// member had no token) with no default, so the fresh-init entity catalog and
/// the backfill agree.
struct MemberActivateColumn;

impl MigrationName for MemberActivateColumn {
    fn name(&self) -> &str {
        "m20260924_000009_member_activate"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for MemberActivateColumn {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE members ADD COLUMN IF NOT EXISTS activate varchar(32)")
            .await?;
        Ok(())
    }
}

/// The merchant settlement-card table (`pay_bankcard`, `spec/05` §10). Its own
/// migration so tracked databases pick it up; every action reads/scans by the
/// owning `userid`, so that column carries an index.
struct Bankcards;

impl MigrationName for Bankcards {
    fn name(&self) -> &str {
        "m20260922_000016_bankcards"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Bankcards {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260922_000016_bankcards", DatabaseBackend::Postgres)
            .table::<data::bankcards::Entity>()
            .build();
        create.up(manager).await?;
        create_index(manager, "idx_bankcards_userid", "bankcards", &["userid"]).await
    }
}

/// The merchant KYC-attachment table (`pay_attachment`, `spec/05` §8.5). Its
/// own migration so tracked databases pick it up; the authorized() page lists a
/// merchant's evidence by `userid`, so that column carries an index.
struct Attachments;

impl MigrationName for Attachments {
    fn name(&self) -> &str {
        "m20260924_000001_attachments"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Attachments {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260924_000001_attachments", DatabaseBackend::Postgres)
            .table::<data::attachments::Entity>()
            .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_attachments_userid",
            "attachments",
            &["userid"],
        )
        .await
    }
}

/// The login audit-log table (`pay_loginrecord`, `spec/05` §5.4). Its own
/// migration so tracked databases pick it up; the merchant page scans its own
/// rows by `(userid, type)`, so those columns carry a composite index.
struct Loginrecords;

impl MigrationName for Loginrecords {
    fn name(&self) -> &str {
        "m20260924_000002_loginrecords"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Loginrecords {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260924_000002_loginrecords", DatabaseBackend::Postgres)
            .table::<data::loginrecords::Entity>()
            .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_loginrecords_userid_type",
            "loginrecords",
            &["userid", "type"],
        )
        .await
    }
}

/// The platform公告 / news table (`pay_article`, `spec/05` §10 console). Its own
/// migration so tracked databases pick it up; the console lists the visible
/// notices by `(status, groupid)` newest-first, so those columns carry an index.
struct Articles;

impl MigrationName for Articles {
    fn name(&self) -> &str {
        "m20260924_000004_articles"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Articles {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260924_000004_articles", DatabaseBackend::Postgres)
            .table::<data::articles::Entity>()
            .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_articles_status_groupid",
            "articles",
            &["status", "groupid"],
        )
        .await
    }
}

/// The per-order投诉保证金 freeze ledger (`pay_complaints_deposit`, `spec/05`
/// §10). Its own migration so tracked databases pick it up; the console sums a
/// merchant's still-frozen balance and the明细 page lists its own rows, both by
/// `(user_id, status)`, so those columns carry a composite index.
struct ComplaintsDeposits;

impl MigrationName for ComplaintsDeposits {
    fn name(&self) -> &str {
        "m20260924_000005_complaints_deposits"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ComplaintsDeposits {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new(
            "m20260924_000005_complaints_deposits",
            DatabaseBackend::Postgres,
        )
        .table::<data::complaints_deposits::Entity>()
        .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_complaints_deposits_user_status",
            "complaints_deposits",
            &["user_id", "status"],
        )
        .await
    }
}

/// The complaints-deposit RULE table (`pay_complaints_deposit_rule`,
/// `spec/02` §4.4) the settle path resolves its withholding from: the
/// merchant's own row, else the `is_system` fallback. Both lookups ride
/// their own indexes.
struct ComplaintsDepositRules;

impl MigrationName for ComplaintsDepositRules {
    fn name(&self) -> &str {
        "m20260925_000001_complaints_deposit_rules"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for ComplaintsDepositRules {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new(
            "m20260925_000001_complaints_deposit_rules",
            DatabaseBackend::Postgres,
        )
        .table::<data::complaints_deposit_rules::Entity>()
        .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_complaints_deposit_rules_user",
            "complaints_deposit_rules",
            &["user_id"],
        )
        .await?;
        create_index(
            manager,
            "idx_complaints_deposit_rules_system",
            "complaints_deposit_rules",
            &["is_system"],
        )
        .await
    }
}

/// The outbound-notify audit table (`spec/02` §4.6 — the legacy wrote a file
/// line per attempt; the rewrite's queryable equivalent). Its own migration;
/// the reissue/notify history reads by `order_id`, so that column carries an
/// index.
struct NotifyLogs;

impl MigrationName for NotifyLogs {
    fn name(&self) -> &str {
        "m20260925_000002_notify_logs"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for NotifyLogs {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260925_000002_notify_logs", DatabaseBackend::Postgres)
            .table::<data::notify_logs::Entity>()
            .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_notify_logs_order",
            "notify_logs",
            &["order_id"],
        )
        .await
    }
}

/// The manual-reversal ledger (`pay_redo_order`, `spec/02` §6.5/§11 — the
/// write side the legacy never had). Its own migration; the statistics
/// aggregate by `(user_id, date)` (`spec/02` §11.4), so that pair carries an
/// index.
struct RedoOrders;

impl MigrationName for RedoOrders {
    fn name(&self) -> &str {
        "m20260925_000003_redo_orders"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for RedoOrders {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260925_000003_redo_orders", DatabaseBackend::Postgres)
            .table::<data::redo_orders::Entity>()
            .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_redo_orders_user_date",
            "redo_orders",
            &["user_id", "date"],
        )
        .await
    }
}

/// The singleton SMS-channel config (`pay_sms`, `spec/05` §5). Its own migration
/// so tracked databases pick it up; the row is read whole (no `where`), so no
/// index is needed.
struct SmsConfigs;

impl MigrationName for SmsConfigs {
    fn name(&self) -> &str {
        "m20260924_000006_sms_configs"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for SmsConfigs {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260924_000006_sms_configs", DatabaseBackend::Postgres)
            .table::<data::sms_configs::Entity>()
            .build();
        create.up(manager).await
    }
}

/// The per-scene SMS template lookup (`pay_sms_template`, `spec/05` §5). Its own
/// migration; `getSmsTemplateCode` fetches by `call_index`, so that column
/// carries an index.
struct SmsTemplates;

impl MigrationName for SmsTemplates {
    fn name(&self) -> &str {
        "m20260924_000007_sms_templates"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for SmsTemplates {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260924_000007_sms_templates", DatabaseBackend::Postgres)
            .table::<data::sms_templates::Entity>()
            .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_sms_templates_call_index",
            "sms_templates",
            &["call_index"],
        )
        .await
    }
}

/// The找回密码 email-code ledger (`pay_user_code`, `spec/05` §5). Its own
/// migration so tracked databases pick it up; `forgetpwd` matches a live code by
/// `(username, email)`, so those columns carry an index.
struct UserCodes;

impl MigrationName for UserCodes {
    fn name(&self) -> &str {
        "m20260924_000008_user_codes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for UserCodes {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let create = EntityTables::new("m20260924_000008_user_codes", DatabaseBackend::Postgres)
            .table::<data::user_codes::Entity>()
            .build();
        create.up(manager).await?;
        create_index(
            manager,
            "idx_user_codes_username_email",
            "user_codes",
            &["username", "email"],
        )
        .await
    }
}

async fn create_index(
    mgr: &SchemaManager<'_>,
    name: &str,
    table: &str,
    cols: &[&str],
) -> Result<(), DbErr> {
    let mut idx = Index::create();
    idx.name(name).table(Alias::new(table));
    for c in cols {
        idx.col(Alias::new(*c));
    }
    mgr.create_index(idx).await
}

/// Applies pending migrations against the configured DSN. Called from
/// `main` before the server boots.
pub async fn run(cfg: &crate::config::Config) -> Result<(), String> {
    let mut opts = ConnectOptions::new(cfg.database_source.clone());
    opts.connect_timeout(std::time::Duration::from_secs(10));
    let db = sea_orm::Database::connect(opts)
        .await
        .map_err(|e| format!("postgres connect (migrate): {e}"))?;
    migrate(&db).await
}

/// Applies pending migrations on an already-open connection (the startup
/// path above and the DB-gated integration tests share this).
pub async fn migrate(db: &sea_orm::DatabaseConnection) -> Result<(), String> {
    Migrator::up(db, None)
        .await
        .map_err(|e| format!("schema migrate: {e}"))
}
