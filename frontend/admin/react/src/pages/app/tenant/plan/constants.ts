/**
 * 套餐模块枚举映射常量
 * 页面和 Drawer 共用
 */

type TFn = (key: string, options?: Record<string, any>) => string;

// ========== 套餐版本 ==========

export const PLAN_VERSION_COLORS: Record<string, string> = {
  FREE: 'default',
  STANDARD: 'processing',
  ENTERPRISE: 'warning',
};

export const PLAN_VERSION_STATUS: Record<string, string> = {
  FREE: 'Default',
  STANDARD: 'Processing',
  ENTERPRISE: 'Warning',
};

export function getPlanVersionMap(t: TFn) {
  return {
    FREE: { text: t('version.FREE'), color: PLAN_VERSION_COLORS.FREE },
    STANDARD: { text: t('version.STANDARD'), color: PLAN_VERSION_COLORS.STANDARD },
    ENTERPRISE: { text: t('version.ENTERPRISE'), color: PLAN_VERSION_COLORS.ENTERPRISE },
  };
}

export function getPlanVersionOptions(t: TFn) {
  return [
    { label: t('version.FREE'), value: 'FREE' },
    { label: t('version.STANDARD'), value: 'STANDARD' },
    { label: t('version.ENTERPRISE'), value: 'ENTERPRISE' },
  ];
}

// ========== 到期处置策略 ==========

export const EXPIRY_POLICY_COLORS: Record<string, string> = {
  READONLY: 'warning',
  BLOCK_LOGIN: 'error',
  FREEZE: 'default',
};

export const EXPIRY_POLICY_STATUS: Record<string, string> = {
  READONLY: 'Warning',
  BLOCK_LOGIN: 'Error',
  FREEZE: 'Default',
};

export function getExpiryPolicyMap(t: TFn) {
  return {
    READONLY: { text: t('expiryPolicy.READONLY'), color: EXPIRY_POLICY_COLORS.READONLY },
    BLOCK_LOGIN: { text: t('expiryPolicy.BLOCK_LOGIN'), color: EXPIRY_POLICY_COLORS.BLOCK_LOGIN },
    FREEZE: { text: t('expiryPolicy.FREEZE'), color: EXPIRY_POLICY_COLORS.FREEZE },
  };
}

export function getExpiryPolicyOptions(t: TFn) {
  return [
    { label: t('expiryPolicy.READONLY'), value: 'READONLY' },
    { label: t('expiryPolicy.BLOCK_LOGIN'), value: 'BLOCK_LOGIN' },
    { label: t('expiryPolicy.FREEZE'), value: 'FREEZE' },
  ];
}

// ========== 配额类型 ==========

export const QUOTA_TYPE_COLORS: Record<string, string> = {
  USER_LIMIT: 'processing',
  STORAGE: 'warning',
  API_CALL: 'default',
};

export const QUOTA_TYPE_STATUS: Record<string, string> = {
  USER_LIMIT: 'Processing',
  STORAGE: 'Warning',
  API_CALL: 'Default',
};

export function getQuotaTypeMap(t: TFn) {
  return {
    USER_LIMIT: { text: t('quotaType.USER_LIMIT'), color: QUOTA_TYPE_COLORS.USER_LIMIT },
    STORAGE: { text: t('quotaType.STORAGE'), color: QUOTA_TYPE_COLORS.STORAGE },
    API_CALL: { text: t('quotaType.API_CALL'), color: QUOTA_TYPE_COLORS.API_CALL },
  };
}

export function getQuotaTypeOptions(t: TFn) {
  return [
    { label: t('quotaType.USER_LIMIT'), value: 'USER_LIMIT' },
    { label: t('quotaType.STORAGE'), value: 'STORAGE' },
    { label: t('quotaType.API_CALL'), value: 'API_CALL' },
  ];
}

// ========== 通用 Select fieldProps ==========

export const SELECT_FILTER_PROPS = {
  showSearch: true,
  allowClear: true,
  filterOption: (input: string, option: any) =>
    (option?.label ?? '').toLowerCase().includes(input.toLowerCase()),
};

// ========== 模块白名单（多选） ==========
// 用于套餐 drawer 中的模块白名单多选框，选项标签来自 i18n module.* 枚举。
// 值对齐 identityservicev1_Module 枚举（数字），由调用方与后端 PlanModule.entity 交互。

export const MODULE_LABEL_KEYS: string[] = [
  'DASHBOARD',
  'OPM',
  'SYSTEM',
  'DICT',
  'TENANT',
  'PERMISSION',
  'LOG',
  'INTERNAL_MESSAGE',
  'FILE',
  'TASK',
];

export function getModuleOptions(t: TFn) {
  return MODULE_LABEL_KEYS.map((k) => ({
    label: t(`module.${k}`),
    value: k,
  }));
}
