/** A host that echoes requests back, so a secret sent to it comes back to the agent. */
export function echoHost(host: string) {
  const name = host.trim().toLowerCase();
  const under = (domain: string) => name === domain || name.endsWith(`.${domain}`);
  return under("httpbin.org") || under("postman-echo.com") || name.startsWith("echo.");
}
