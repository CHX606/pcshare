import type { ApiErrorBody, ErrorCode } from './types'

export class ApiError extends Error {
  readonly code: ErrorCode
  constructor(code: ErrorCode, msg: string) {
    super(msg)
    this.code = code
  }
  get body(): ApiErrorBody {
    return { code: this.code, msg: this.message }
  }
}

/** 没有附带说明时，按错误码给出的人话。 */
const FALLBACK: Record<ErrorCode, string> = {
  unauthorized: '对方还没有和这台电脑配对',
  forbidden: '对方没有授权这项操作，可以在对方电脑的设备设置里打开',
  not_found: '找不到这个文件或文件夹，可能已被移走或删除',
  already_exists: '同名的文件或文件夹已经存在',
  busy: '文件正被其他程序占用，关掉相关程序后再试',
  invalid: '填写的内容不对，检查后再试',
  unsupported: '对方的程序版本太旧，不支持这项功能，请先升级对方',
  io: '读写磁盘时出错',
  cancelled: '已取消',
  internal: '程序内部出错，可以在诊断页导出日志反馈',
  offline: '对方现在不在线，等它上线后再试',
  timeout: '对方迟迟没有回应，检查网络后再试',
  bad_code: '配对码不对',
  expired: '配对码已过期，请让对方重新生成',
}

/** 把任何异常变成一句给人看的话。 */
export function humanError(err: unknown): string {
  if (err instanceof ApiError) return err.message || FALLBACK[err.code]
  if (isErrorBody(err)) return err.msg || FALLBACK[err.code] || FALLBACK.internal
  return FALLBACK.internal
}

export function errorCode(err: unknown): ErrorCode {
  if (err instanceof ApiError) return err.code
  if (isErrorBody(err)) return err.code
  return 'internal'
}

export function isErrorBody(v: unknown): v is ApiErrorBody {
  return typeof v === 'object' && v !== null && 'code' in v && typeof (v as ApiErrorBody).code === 'string'
}

export function toApiError(v: unknown): ApiError {
  if (v instanceof ApiError) return v
  if (isErrorBody(v)) return new ApiError(v.code, v.msg || FALLBACK[v.code] || FALLBACK.internal)
  return new ApiError('internal', FALLBACK.internal)
}
