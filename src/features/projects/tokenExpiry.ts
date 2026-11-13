export const expiresSoon = (expiresAt: number, now = Date.now()) =>
  expiresAt - now <= 30 * 24 * 60 * 60 * 1000;
