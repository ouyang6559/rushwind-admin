import { useEffect, useState } from 'react';
import { Modal, Button, Input, Alert, Descriptions, Space, Typography, App } from 'antd';
import { DeleteOutlined, PlayCircleOutlined, PlusOutlined } from '@ant-design/icons';
import { useTranslation } from 'react-i18next';
import type {
  scriptservicev1_Script as Script,
  scriptservicev1_TestRunScriptResponse,
} from '@/api/generated/admin/service/v1';
import { useTestRunScript } from '@/api/hooks/script';
import { type TestRunInputRow } from '../constants';

interface TestRunModalProps {
  open: boolean;
  /** 待试运行的已保存脚本；为空时提示须先选择脚本 */
  script?: Script;
  onClose: () => void;
  /** 运行成功后回调（列表可能需要刷新版本号） */
  onSuccess?: () => void;
}

/**
 * 脚本试运行对话框：
 * 以 id 模式调用后端 TestRun（在服务端一次性隔离引擎中执行）。
 * 支持配置若干「键 → JSON 值」的上下文输入。
 */
const TestRunModal: React.FC<TestRunModalProps> = ({ open, script, onClose, onSuccess }) => {
  const { t } = useTranslation('script');
  const { message } = App.useApp();

  const [inputRows, setInputRows] = useState<TestRunInputRow[]>([]);
  const [result, setResult] = useState<scriptservicev1_TestRunScriptResponse | undefined>();
  const [hadInvalidJson, setHadInvalidJson] = useState(false);

  const testRunMutation = useTestRunScript({
    onSuccess: (resp) => {
      setResult(resp);
      if (resp.success) {
        onSuccess?.();
      }
    },
    onError: (error: Error) => {
      message.error(error.message || t('fetchFailed'));
    },
  });

  // 打开时重置
  useEffect(() => {
    if (open) {
      setInputRows([]);
      setResult(undefined);
      setHadInvalidJson(false);
    }
  }, [open]);

  const updateRow = (index: number, patch: Partial<TestRunInputRow>) => {
    setInputRows((rows) => rows.map((row, i) => (i === index ? { ...row, ...patch } : row)));
  };

  const handleRun = () => {
    if (!script?.id) return;

    // 组装 input：值解析为 JSON；解析失败按原样字符串传入（后端同样有该兜底）
    const input: Record<string, string> = {};
    let invalid = false;
    for (const row of inputRows) {
      const key = row.key.trim();
      if (!key) continue;
      input[key] = row.value;
      if (row.value.trim() !== '') {
        try {
          JSON.parse(row.value);
        } catch {
          invalid = true;
        }
      }
    }
    setHadInvalidJson(invalid);

    testRunMutation.mutate({ id: script.id, input });
  };

  const contextEntries = Object.entries(result?.context || {});

  return (
    <Modal
      title={t('testRunTitle', { name: script?.name || '-' })}
      open={open}
      onCancel={onClose}
      width={640}
      footer={
        <Space>
          <Button onClick={onClose}>{t('common:button.cancel')}</Button>
          <Button
            type="primary"
            icon={<PlayCircleOutlined />}
            loading={testRunMutation.isPending}
            disabled={!script?.id}
            onClick={handleRun}
          >
            {t('testRunRun')}
          </Button>
        </Space>
      }
    >
      <Descriptions size="small" column={2} style={{ marginBottom: 16 }}>
        <Descriptions.Item label={t('language')}>{script?.language || '-'}</Descriptions.Item>
        <Descriptions.Item label={t('hookPoint')}>{script?.hookPoint || t('unmounted')}</Descriptions.Item>
      </Descriptions>

      <Typography.Paragraph type="secondary" style={{ marginBottom: 8 }}>
        {t('testRunInput')}
      </Typography.Paragraph>

      {inputRows.map((row, index) => (
        <Space.Compact key={index} style={{ display: 'flex', marginBottom: 8 }}>
          <Input
            style={{ width: 180 }}
            placeholder={t('testRunInputKey')}
            value={row.key}
            onChange={(e) => updateRow(index, { key: e.target.value })}
          />
          <Input
            placeholder={t('testRunInputValue')}
            value={row.value}
            onChange={(e) => updateRow(index, { value: e.target.value })}
          />
          <Button
            icon={<DeleteOutlined />}
            onClick={() => setInputRows((rows) => rows.filter((_, i) => i !== index))}
          />
        </Space.Compact>
      ))}

      <Button
        type="dashed"
        block
        icon={<PlusOutlined />}
        style={{ marginBottom: 16 }}
        onClick={() => setInputRows((rows) => [...rows, { key: '', value: '' }])}
      >
        {t('testRunAddInput')}
      </Button>

      {hadInvalidJson && (
        <Alert type="warning" showIcon title={t('testRunInvalidJson')} style={{ marginBottom: 12 }} />
      )}

      {result && (
        <div>
          <Alert
            type={result.success ? 'success' : 'error'}
            showIcon
            title={result.success ? t('testRunSuccess') : t('testRunFailed')}
            description={
              !result.success && result.error ? (
                <pre style={{ margin: 0, whiteSpace: 'pre-wrap' }}>{result.error}</pre>
              ) : undefined
            }
            style={{ marginBottom: 12 }}
          />
          <Typography.Paragraph style={{ marginBottom: 4 }}>
            {t('testRunOutput')} · {t('testRunDuration')}: {result.durationMs ?? 0}ms
          </Typography.Paragraph>
          <pre
            style={{
              margin: 0,
              maxHeight: 220,
              overflow: 'auto',
              padding: 12,
              borderRadius: 8,
              fontSize: 12,
              background: 'var(--ant-color-fill-quaternary)',
            }}
          >
            {contextEntries.length === 0
              ? t('testRunNoOutput')
              : JSON.stringify(
                  Object.fromEntries(contextEntries.map(([k, v]) => [k, safeParse(v)])),
                  null,
                  2,
                )}
          </pre>
        </div>
      )}
    </Modal>
  );
};

/** 后端上下文值是 JSON 字符串，展示时还原为对象字面量 */
function safeParse(value: string): unknown {
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

export default TestRunModal;
