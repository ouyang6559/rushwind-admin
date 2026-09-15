import { useMutation, useQuery, type UseMutationOptions } from '@tanstack/react-query';
import {
  type authenticationservicev1_DisableMFARequest,
  type authenticationservicev1_GetMFAStatusResponse,
  type authenticationservicev1_StartEnrollMethodRequest,
  type authenticationservicev1_StartEnrollMethodResponse,
  type authenticationservicev1_ConfirmEnrollMethodRequest,
  type authenticationservicev1_ConfirmEnrollMethodResponse,
} from '@/api/generated/admin/service/v1';
import { apiClient } from '@/api/client';

/**
 * 获取当前用户 MFA 状态（是否已绑定、已注册因子列表，不含 secret）。
 */
async function getMfaStatus(): Promise<authenticationservicev1_GetMFAStatusResponse | null> {
  return apiClient.mfaService.GetMFAStatus({});
}

export function useGetMfaStatus(enabled = true) {
  return useQuery({
    queryKey: ['mfa-status'],
    queryFn: () => getMfaStatus(),
    enabled,
  });
}

/**
 * 开始注册 TOTP（后端返回 secret + otpauth URI + QR dataURI + operation_id）。
 */
export function useStartEnrollMfa(
  options?: UseMutationOptions<
    authenticationservicev1_StartEnrollMethodResponse,
    Error,
    authenticationservicev1_StartEnrollMethodRequest
  >,
) {
  return useMutation({
    mutationFn: (req: authenticationservicev1_StartEnrollMethodRequest) =>
      apiClient.mfaService.StartEnrollMethod(req),
    ...options,
  });
}

/**
 * 确认注册 TOTP：提交首码完成绑定。
 */
export function useConfirmEnrollMfa(
  options?: UseMutationOptions<
    authenticationservicev1_ConfirmEnrollMethodResponse,
    Error,
    authenticationservicev1_ConfirmEnrollMethodRequest
  >,
) {
  return useMutation({
    mutationFn: (req: authenticationservicev1_ConfirmEnrollMethodRequest) =>
      apiClient.mfaService.ConfirmEnrollMethod(req),
    ...options,
  });
}

/**
 * 禁用/移除当前用户的 MFA 凭证。
 */
export function useDisableMfa(
  options?: UseMutationOptions<{}, Error, authenticationservicev1_DisableMFARequest>,
) {
  return useMutation({
    mutationFn: (req: authenticationservicev1_DisableMFARequest) =>
      apiClient.mfaService.DisableMFA(req),
    ...options,
  });
}

/**
 * 管理端救援重置：清空目标用户指定方法（TOTP）的全部 MFA 因子。
 * 仅平台管理员可调用（后端强制校验）。
 */
export function useAdminResetMfa(
  options?: UseMutationOptions<{}, Error, authenticationservicev1_DisableMFARequest>,
) {
  return useMutation({
    mutationFn: (req: authenticationservicev1_DisableMFARequest) =>
      apiClient.mfaService.DisableMFA(req),
    ...options,
  });
}
