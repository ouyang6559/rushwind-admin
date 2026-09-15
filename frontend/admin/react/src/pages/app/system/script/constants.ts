import type { scriptservicev1_Language } from '@/api/generated/admin/service/v1';

/** 语言选项（ProFormRadio / ProTable 筛选共用） */
export const getLanguageOptions = (t: (key: string) => string) => [
  { label: t('languageLua'), value: 'LUA' },
  { label: t('languageJavascript'), value: 'JAVASCRIPT' },
];

export const languageColorMap: Record<scriptservicev1_Language, string> = {
  LUA: 'blue',
  JAVASCRIPT: 'purple',
};

/** 试运行输入键值对的行类型 */
export interface TestRunInputRow {
  key: string;
  value: string;
}
