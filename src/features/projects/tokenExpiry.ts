/** Without an entered date, the app counts one year from the first save of this value. */
export const ESTIMATE =
  "Estimate: one year after you saved this token. Enter the date that claude setup-token prints to make it exact.";

export const expiresSoon = (expiresAt: number, now = Date.now()) =>
  expiresAt - now <= 30 * 24 * 60 * 60 * 1000;

export const expiryLabel = (expiresAt: number, exact: boolean) =>
  `expires ${exact ? "" : "about "}${new Date(expiresAt).toLocaleDateString()}`;

/** The start of the local day of a `YYYY-MM-DD` date input, or null when it is empty. */
export const dayStart = (date: string) => {
  if (!date) return null;
  const [year, month, day] = date.split("-").map(Number);
  return new Date(year, month - 1, day).getTime();
};
