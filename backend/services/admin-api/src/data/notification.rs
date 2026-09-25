//! Notification entity tables: channels, routing rules, the delivery ledger.

pub mod sys_notification_channels {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sys_notification_channels")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub name: String,
        /// enum(EMAIL,WEBHOOK) default EMAIL.
        #[sea_orm(column_name = "type")]
        pub type_column: Option<String>,
        pub smtp_host: Option<String>,
        pub smtp_port: Option<u32>,
        pub smtp_username: Option<String>,
        pub smtp_password: Option<String>,
        pub smtp_from: Option<String>,
        /// enum(NONE,START_TLS,SSL) default START_TLS.
        pub smtp_tls: Option<String>,
        pub webhook_url: Option<String>,
        pub webhook_secret: Option<String>,
        /// enum(CUSTOM,NONE,DINGTALK,FEISHU,WECOM).
        pub webhook_sign_style: Option<String>,
        pub webhook_payload_template: Option<String>,
        pub remark: Option<String>,
        /// enum(OFF,ON) default ON.
        pub status: Option<String>,
        pub created_by: Option<u32>,
        pub updated_by: Option<u32>,
        pub deleted_by: Option<u32>,
        pub created_at: Option<chrono::NaiveDateTime>,
        pub updated_at: Option<chrono::NaiveDateTime>,
        pub deleted_at: Option<chrono::NaiveDateTime>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
pub mod sys_notification_rules {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sys_notification_rules")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        /// enum(PASSWORD_RESET_CODE,CONTACT_BIND_CODE,CHANNEL_TEST_EMAIL,INTERNAL_MESSAGE) unique.
        pub event_type: Option<String>,
        /// enum(EMAIL,SMS,WEBHOOK,INTERNAL).
        pub channel: Option<String>,
        #[sea_orm(default_value = false)]
        pub is_async: Option<bool>,
        #[sea_orm(default_value = true)]
        pub is_enabled: Option<bool>,
        pub remark: Option<String>,
        pub created_by: Option<u32>,
        pub updated_by: Option<u32>,
        pub deleted_by: Option<u32>,
        pub created_at: Option<chrono::NaiveDateTime>,
        pub updated_at: Option<chrono::NaiveDateTime>,
        pub deleted_at: Option<chrono::NaiveDateTime>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
pub mod sys_notification_deliveries {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sys_notification_deliveries")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        /// enum(PASSWORD_RESET_CODE,CONTACT_BIND_CODE,CHANNEL_TEST_EMAIL,INTERNAL_MESSAGE).
        pub event_type: Option<String>,
        /// enum(EMAIL,SMS,WEBHOOK,INTERNAL).
        pub channel: Option<String>,
        pub channel_id: Option<u32>,
        pub recipient_user_id: Option<u32>,
        pub related_id: Option<u32>,
        pub target: Option<String>,
        /// enum(SENDING,SENT,FAILED,SKIPPED) default SENDING.
        pub status: Option<String>,
        pub last_error: Option<String>,
        pub request_id: Option<String>,
        #[sea_orm(default_value = 0)]
        pub attempts: Option<u32>,
        pub created_by: Option<u32>,
        pub updated_by: Option<u32>,
        pub deleted_by: Option<u32>,
        pub created_at: Option<chrono::NaiveDateTime>,
        pub updated_at: Option<chrono::NaiveDateTime>,
        pub deleted_at: Option<chrono::NaiveDateTime>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
