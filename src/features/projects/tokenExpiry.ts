/** Without an entered date, the app counts one year from the first save of this value. */
export const ESTIMATE =
  "Estimate: one year after you saved this token. Enter its expiry date to make it exact.";

export const expiresSoon = (expiresAt: number, now = Date.now()) =>
  expiresAt - now <= 30 * 24 * 60 * 60 * 1000;

/** Shows the last day on which the token is valid: an entered date ends at the next midnight. */
export const expiryLabel = (expiresAt: number, exact: boolean) =>
  `expires ${exact ? "" : "about "}${new Date(expiresAt - 1).toLocaleDateString()}`;

/**
 * The end of the local day of a `YYYY-MM-DD` date input (the next local midnight), or null when it is empty.
 * The token stays valid through the entered day.
 */
export const dayEnd = (date: string) => {
  if (!date) return null;
  const [year, month, day] = date.split("-").map(Number);
  return new Date(year, month - 1, day + 1).getTime();
};
