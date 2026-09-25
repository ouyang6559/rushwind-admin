//! Internal-message entity tables: messages, categories, per-user recipients.

pub mod internal_messages {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "internal_messages")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub tenant_id: Option<u32>,
        pub title: Option<String>,
        pub content: Option<String>,
        pub sender_id: Option<u32>,
        pub category_id: Option<u32>,
        /// enum(DRAFT,PUBLISHED,SCHEDULED,REVOKED,ARCHIVED,DELETED) default DRAFT.
        pub status: Option<String>,
        /// enum(NOTIFICATION,PRIVATE,GROUP) default NOTIFICATION.
        #[sea_orm(column_name = "type")]
        pub type_column: Option<String>,
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
pub mod internal_message_categories {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "internal_message_categories")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub tenant_id: Option<u32>,
        pub name: String,
        pub code: String,
        pub icon_url: Option<String>,
        #[sea_orm(default_value = true)]
        pub is_enabled: Option<bool>,
        #[sea_orm(default_value = 0)]
        pub sort_order: Option<u32>,
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
pub mod internal_message_recipients {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "internal_message_recipients")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub tenant_id: Option<u32>,
        pub message_id: Option<u32>,
        pub recipient_user_id: Option<u32>,
        /// enum(SENT,RECEIVED,READ,REVOKED,DELETED) default SENT.
        pub status: Option<String>,
        pub received_at: Option<chrono::NaiveDateTime>,
        pub read_at: Option<chrono::NaiveDateTime>,
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
