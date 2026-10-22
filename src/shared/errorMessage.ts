/** The backend rejects with `{ code, message }`, where `message` is already written for the user. */
interface BackendError {
  code: string;
  message: string;
}

export const isBackendError = (e: unknown): e is BackendError =>
  typeof e === "object" && e !== null && "code" in e && "message" in e && typeof e.message === "string";

/** The text that the user reads for any error. */
export function errorMessage(e: unknown): string {
  if (isBackendError(e) || e instanceof Error) return e.message;
  return String(e);
}

/** The backend's error variant, such as `ShieldsRunning`. */
export const errorCode = (e: unknown) => (isBackendError(e) ? e.code : undefined);
