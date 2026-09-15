import { uploadFile } from '@/api/hooks/file-transfer';

/**
 * 编辑器内置图片上传：走文件上传 API（服务端中转 + 元数据落库），
 * 取后端生成的签名公开访问 URL（publicUrl）供富文本内嵌预览。
 * GOWIND_CRYPTO_KEY 未配置时 publicUrl 为空，返回空串由调用方提示失败。
 */
export async function defaultImageUpload(file: File): Promise<string> {
  const resp = (await uploadFile('', '', file)) as any;
  return resp?.publicUrl || '';
}

/** 从剪贴板/拖拽数据中提取图片文件 */
export function extractImageFiles(files: FileList | File[] | undefined | null): File[] {
  if (!files) return [];
  return Array.from(files).filter((f) => f.type.startsWith('image/'));
}
