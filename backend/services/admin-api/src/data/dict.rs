//! Dict / i18n entity tables.

pub mod sys_dict_types {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sys_dict_types")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub tenant_id: Option<u32>,
        pub type_code: String,
        pub type_name: String,
        #[sea_orm(default_value = true)]
        pub is_enabled: Option<bool>,
        #[sea_orm(default_value = 0)]
        pub sort_order: Option<u32>,
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
pub mod sys_dict_entries {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sys_dict_entries")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub tenant_id: Option<u32>,
        pub type_id: Option<u32>,
        pub entry_value: Option<String>,
        pub numeric_value: Option<i32>,
        #[sea_orm(default_value = true)]
        pub is_enabled: Option<bool>,
        #[sea_orm(default_value = 0)]
        pub sort_order: Option<u32>,
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
pub mod sys_dict_entry_i18n {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sys_dict_entry_i18n")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub tenant_id: Option<u32>,
        pub entry_id: Option<u32>,
        pub language_code: Option<String>,
        pub entry_label: Option<String>,
        pub description: Option<String>,
        #[sea_orm(default_value = 0)]
        pub sort_order: Option<u32>,
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
pub mod sys_languages {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sys_languages")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: u32,
        pub language_code: String,
        pub language_name: String,
        pub native_name: Option<String>,
        #[sea_orm(default_value = false)]
        pub is_default: Option<bool>,
        #[sea_orm(default_value = true)]
        pub is_enabled: Option<bool>,
        #[sea_orm(default_value = 0)]
        pub sort_order: Option<u32>,
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
