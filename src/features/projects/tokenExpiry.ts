/** The app cannot read the expiry of a token. It counts one year from the first save of this value. */
export const ESTIMATE =
  "Estimate: one year after you saved this token. claude setup-token prints the exact date when it makes the token.";

export const expiresSoon = (expiresAt: number, now = Date.now()) =>
  expiresAt - now <= 30 * 24 * 60 * 60 * 1000;
