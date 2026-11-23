---
title: Network
description: Control the hosts that the agent can reach, and give it credentials.
---

All network traffic of a cage goes through the proxy of its project. The VM can reach only the proxy. Connections from
the outside into the cage are always blocked.

## Network policy

The network policy belongs to the cage of a project: the projects of one cage share it. Open it with ⌘4.

- **Default**: Block, Ask or Allow all hosts that no rule names. See [Ask](#ask).
- **Rules**: each rule names a service of the catalog (for example GitHub or npm, with its domains) or a host, and says
  Allow or Block. A host is `a.io`, or `*.a.io` for the domain and its subdomains.
- **Methods**: an Allow rule can allow only some HTTP methods. See [Method limits](#method-limits).
- **Offline**: blocks all requests now and keeps the rules.

The proxy checks each request in this sequence:

1. Offline.
2. The service on this Mac, unless a Block rule names `host.microsandbox.internal`.
3. Addresses that are always blocked: this Mac, the local network, loopback, link-local ranges and cloud metadata, except the port in step 2.
4. Block rules.
5. Allow rules, and the rules [until the VM stops](#allow-until-the-vm-stops). A method that a matched rule does not
   allow is blocked.
6. The default.

A Block rule always has priority over an Allow rule. A rule change applies to the next request, with no restart.

### Ask

With the default **Ask for everything else**, the proxy holds a request that no rule decides. The request shows in
**Requests that wait for you** on the network page of the session, with its method, host and port. Select one answer:

- **Allow once**: allows this request. The later requests of the same HTTPS connection get the same answer.
- **Allow until stop**: adds an Allow rule for the host [until the VM stops](#allow-until-the-vm-stops).
- **Always allow**: saves an Allow rule for the host in the cage.
- **Block**: blocks this request, and the later requests of the same HTTPS connection.

The limits:

- A request waits at most 30 seconds. With no answer, it is blocked with the reason `ask-timeout`.
- A VM holds at most 32 waiting requests, and the app at most 128. A request over these limits is blocked at once with
  the reason `ask-timeout`.
- A rule change, Offline or a VM stop decides a waiting request at once.
- Ask applies only to requests through the proxy. The proxy decides an HTTPS connection on each request in it, so the
  method is known. A direct connection that ignores the proxy is still blocked.
- An earlier app version cannot read a cage whose default is Ask. Set the default to Block or Allow before you go back
  to such a version.
- The **Test** button opens an ask too. It waits up to 40 seconds, so you can answer the ask. With no answer, the result
  is "blocked: no answer to the ask in time".

### Allow until the VM stops

The card **Allowed until the VM stops** lists the Allow rules of the session's VM that are not saved. Add a host or a
`*.` domain, or select **Allow until stop** on an ask. The X button removes a rule. These rules apply only to that VM.
They end when the VM stops, and another VM of the same cage never gets them. Block rules, Offline and the
always-blocked groups still have priority. These rules cannot have credentials or method limits.

### Method limits

The method button of an Allow rule shows **All methods** or its methods, for example `GET, HEAD`. Select it to choose
the methods: GET, HEAD, POST, PUT, PATCH, DELETE and OPTIONS. **All methods** clears the list. A request with another
method is blocked with the reason `method`, also under the default Allow or Ask. If more than one Allow rule matches a
host, the request must have a method that each of them allows. A credential of the rule goes only with an allowed
method. The method list stays when you switch a rule between Allow and Block, but a Block rule blocks all methods.

### Profiles

A profile is a named copy of the default and the rules of a cage, with their methods. It has no credentials. Use the
**Profiles** card on the network page:

- **Save current rules** saves the profile. A profile with the same name is replaced.
- **Apply…** copies the profile into the current cage, after you confirm. The default and the rules of the profile
  replace the ones of the cage. A rule of the cage that has a credential stays. If the profile has a rule for the same
  target, the rule gets the action and the methods of the profile and keeps its credentials. Offline and the service on
  this Mac do not change.
- The X button deletes the profile. Cages that got the profile keep their rules.

A change of a profile or of a cage never changes another cage. The app keeps at most 100 profiles in
`network-profiles.json` in the zwinger data folder.

### Policy as JSON

**Effective policy as JSON** in the **Profiles** card shows the policy of the cage as read-only JSON: the order of the
checks, Offline, the always-blocked groups, the service on this Mac, the rules, the rules until stop of each running VM,
the default and the Ask limits. It has the credential names and templates of the rules, never a credential value.

### Allow from the map

On a project's Overview map, drag from the cage gateway to **New host or service**, an eligible exact host node, or
empty space. An exact host node fills in that host. The new target or empty space opens a draft where you can enter a
host or select a service. The **Allow host** button above
the map gives keyboard access to the same draft. Review the target and the current cage, then select **Save Allow
rule**. **Cancel** leaves the rules as they were. A failed save keeps the draft and shows the error. A saved rule is
shown on the map after the configuration updates; a draw alone does not save a rule.

The review tells you when Offline or another Block rule still prevents access. Saving an Allow rule does not turn off
Offline or remove a Block rule. If the target has a Block rule with credentials, the review names those credentials;
you must acknowledge that they become active before you can save the change. The same review opens from **Review Allow**
on a Block rule with credentials. The review edits the project's current
cage, which can differ from the cage of its old running VM. A rule change applies to the next request in the cage that
you edited. A VM that still runs the old cage uses the new cage after a restart.

### Remove a rule

The X button of a rule, or **Remove network rule** in ⌘K, removes the rule at once. The next request does not get the
rule or its credentials. A local credential value that no other rule uses is deleted from the vault. If this delete
fails, the removal is still done: the value stays unused, and the next app start deletes it.

After a removal, a notice with **Undo** shows for up to 10 seconds, counted from when the removal is sent. If the reply
comes too late, no notice shows. **Undo** puts the rule back at its position in the same project's cage, with its
credentials and their stored values. The values are never shown. The undo fails and changes nothing in these cases:

- The rule, or a rule for the same service or host, is in the list again.
- A local credential has a different value now, or its value was saved again, also with the same text. The undo does
  not replace it.
- A credential from the credential library was deleted.
- The Claude Code setup token came from the Claude Code login, and the login is now off. A token that you set on the
  rule yourself comes back.
- The cage has the maximum of 500 rules.

An undo works one time. After 10 seconds from when the removal is sent, or after an app restart, the removal cannot be
undone.

The rules that the network page edits are the rules of the project's current cage. **Test** and **Activity** apply to
the VM that runs for the project. After a cage change, that VM uses its old cage until a restart.

## Service on this Mac

A cage can reach one port of this Mac, for example a local model server. The section **Service on this Mac** on the
network page sets the port. The agent then uses the URL `http://host.microsandbox.internal:<port>`. The page shows the
URL with a copy button.

:::caution
The service acts as your Mac user. The agent can do all that the service lets a local client do. Allow only a port
whose service you trust with the agent.
:::

For example, for Ollama:

1. Start Ollama on the Mac as usual. It listens on `127.0.0.1:11434`. You do not need `OLLAMA_HOST=0.0.0.0`.
2. Set the port to `11434` and confirm.
3. In the cage, use `http://host.microsandbox.internal:11434`, for example
   `OLLAMA_HOST=http://host.microsandbox.internal:11434`.

The limits:

- Plain HTTP only. HTTPS and WebSocket are blocked, with the reason `host-port`.
- For port 80, the URL can omit `:80`. For each other port, the URL must name the port.
- A written port must use one to five ASCII decimal digits whose value is the port, for example `:11434` or `:080` for port 80. A URL with a user
  name (`user@`) is blocked.
- The proxy connects to `127.0.0.1` (IPv4) only. A service that listens only on `::1` or on a LAN address is not
  reachable.
- The proxy adds no credential and no host login.
- A Block rule for `host.microsandbox.internal` and Offline block the port. An Allow rule does not open it.
- **Remove** and Offline block the next request. A response that is already streaming continues to its end.

## Activity

The network page groups the requests by host. The newest host is first. For each host it shows:

- the time, allowed or blocked, and the number of requests.
- the latest request: method, path, status, reason and note. The reason `ask` is your answer to an ask, and
  `ask-timeout` is an ask with no answer. If a tool does not use the proxy, the reason is "direct
  connection blocked — this tool ignores the proxy".
- the credentials that the proxy added.
- **Allow** or **Block** for the host, its wildcard or its service. For the service on this Mac, the host shows as
  `host.microsandbox.internal:<port>`, with **Remove** in place of the rule actions.

The log keeps the names of the query parameters, never their values, headers or bodies.

### Retention

Each VM keeps its log after a stop and after you quit zwinger. The log keeps about the last 1000 to 2000 requests of
the VM. It has two files of at most 1000 requests and 2 MiB each. When the newer file is full, the older file goes.
The log is in the zwinger data folder, in `activity/`, which only your user can read. Each file has a hashed name, so
the name does not show the VM key. If a saved file is over these limits, zwinger reads only about its first 2 MiB. From that
part, it keeps the newest requests that fit, and writes the files again at the next request. Requests after that part of
such a file are lost. The agent in the VM cannot read the log. zwinger makes the folder and the files
private again when it reads them. It does not read or delete a folder or file of a different user. It does not read,
change or write a log file that has a second name (a hard link), because that name can be a file in a different folder.

One request in the log can have at most 16 KiB. zwinger does not log a larger request, and a notice tells you this
once for each VM. The network rules still apply to it.

Each request has a number. At the next start, zwinger skips a saved line with a number above 2^53 − 1. If the last
saved number is that limit, zwinger does not log new requests of the VM, and a notice tells you this once. Remove the
cage or its VM to start a new log.

zwinger writes each request when it happens, but does not wait for the disk. If the Mac or zwinger crashes, the last
requests can be lost. At the next start, zwinger skips a line that is broken and keeps the others. If zwinger cannot
write the log, a notice names the file. The requests then show until you quit zwinger, and the rules still apply.

If zwinger cannot read the saved log, the page shows an error and zwinger does not change the file. New requests show
until you quit zwinger, from number 1, but zwinger does not save them, and a notice tells you this. Fix the file, then
restart zwinger.

The log goes when you delete its cage or its session, or when a session goes back to the shared VM. A new VM with the
same key starts with an empty log.

### Export

**Export** saves the whole log of the session's VM as a JSON file, at a place that you pick. The file has the same
data as the page: hosts, paths with query names only, statuses, sizes, decisions and credential names. It has no
credential values. Check the hosts and paths before you share the file. If zwinger cannot read the saved log, Export
shows an error and writes no file.

## Credentials

A credential is a named value, for example `GITHUB_TOKEN`, that an Allow rule gives to its hosts. The agent sees only
a placeholder. The proxy puts the real value into the request.

- The value is in the vault file, never in the project file. The app can write the value but cannot show it.
- The proxy adds a credential only to HTTPS requests.
- A credential on a Block rule is inactive. The proxy does not use it until the rule allows the host again.
- A wildcard host rule (`*.a.io`) cannot have a credential.
- A change of the value applies to the next request.
- A Claude Code setup token is stored as `CLAUDE_CODE_OAUTH_TOKEN` with its save date. Its estimated expiry is 365 days after save. The proxy uses it only for `api.anthropic.com` through an exact-host Allow rule. Under default Block, the project offers an **Allow api.anthropic.com** action. Block rules take priority over Allow rules.
- Claude setup tokens have no refresh host. Claude cages no longer block `console.anthropic.com` or `platform.claude.com` for this login.

### Shared credentials

The credential library holds values that more than one cage can use. Open **Settings > Credentials** to add, rotate or
delete an entry. The value field is write-only: the app never shows a stored value.

- A new library entry goes to no cage and no host. A rule uses it only after you attach it.
- To attach it, open the rule's credentials, set **Value from** to **Credential library** and pick the entry. Each rule
  shows its source: **library** or **this cage**.
- A cage can have its own value and a library value with the same name, on rules for different hosts. Each host gets
  the value of its own rule. The guest has one placeholder for the name.
- Two Allow rules that send credentials to the same host cannot bind one name to two sources. The app refuses this
  edit, so the value never depends on the rule order.
- A host cannot get one **Set** header from two sources, for example a library `Authorization` header and a cage
  `Authorization` header with different names. The app refuses this edit on one rule and on two Allow rules for the
  same host. The header names are compared without case and spaces. Two credentials of one source can set one header,
  and different hosts can each get the header from a different source.
- A rule from before the library uses the value of its cage.
- **Add** is refused when the name is already in the library, also when another window added it. Use **Rotate** for it.
- If the library list does not load, **Settings** shows the error and **Retry**. **Add** is disabled until the list loads.
- **Rotate** saves a new value once. Each running VM that attaches the entry uses it from its next request.
- **Delete** is refused while a rule of a cage attaches the entry. The error names those cages. Detach the entry there
  first. The value and the attachments stay after a refused delete.
- If an attached library value is missing, the cage does not start. The proxy never uses a value of this cage in its place.
- A detach, a rule removal or a cage delete keeps the library value. An entry that no cage attaches stays after a restart.

### The vault file

The vault file is `~/Library/Application Support/com.driif.zwinger/vault/credentials.json`. It holds the values of all
cages and the library. The file has mode 0600 and its folder has mode 0700. Both must belong to your macOS user. The
app replaces the file in one atomic step at each change. If the file or the folder is not safe, the app refuses to read
or change the vault. It does not truncate, follow or remove such an object. The error gives the path in quotes and the
step that fixes it:

| Problem | Step |
|---|---|
| Other users can read it | In Terminal, run the `chmod` command from the error, then try again. |
| It belongs to another user | Ask an administrator of the Mac to make you its owner, then try again. |
| It is a symbolic link | Move the link to a different folder, then try again. |
| It is not a file or a folder, as expected | Move it to a different folder, then try again. |

The app reads vault formats 1, 2 and 3. Reading an older vault keeps its format. The next write saves it in format 3,
even if you change only a cage credential. Earlier app versions cannot open the upgraded file: they show an error such as
`unknown version 2` or `unknown version 3`. There is no downgrade path: keep this version or a later one, because
they read the file. A save of an unchanged value keeps its saved date. Existing values stay
with their cages. Shared credentials have a separate section in the file.

:::caution
Any process that runs as your macOS user can read the vault file. The file protects the values from other users and
from the cages, not from other programs of your user.
:::

Older versions kept the values in the macOS Keychain. A credential from before shows **Not set** until you import it
or enter its value again. A cage with such a credential does not start until you do one of the two.

### Import from the macOS Keychain

Open **Settings > Credentials**, section **Credentials in the macOS Keychain**. zwinger does not touch the Keychain
until you click a button there. It never reads or changes the Keychain at app start.

1. **Check Keychain** lists the old items (service `zwinger`) and tells what an import does with each. It reads no
   value, changes nothing and does not allow a removal.
2. **Import from Keychain** copies each value to the cage with the same project id, if that cage still has its own
   credential with that name, and saves an import receipt. macOS can ask for your password or for **Allow** one time
   for each item. If you cancel a prompt, that item stays, and the import continues with the others. Click Import
   again to try again.
3. **Remove from Keychain** is a separate step. Only items from a saved import receipt can be removed, also after a
   restart. zwinger reads each item again and removes it only if the item did not change after the import and the vault
   holds the same value, saved to disk. macOS can ask again for each item.

An item that changed after the import, or a new item with the same name, stays until you import again. If more than one
Keychain item has the same name, zwinger does not read or remove any of them. A new import takes back the removal of
each item that it cannot read or find.

The import never replaces a value in the vault and never removes a Keychain item. The report gives one result for each
item:

| Result | Meaning |
|---|---|
| Imported | The vault has the value now. |
| Already in the vault | The vault had the same value. Nothing changed. |
| Different value | The vault has a different value. Both values stay. |
| Not imported | No cage has this project id, the cage has no own credential with this name, the cage takes the name from the credential library, or the name is a host login. The item stays. |
| Old proxy CA key | `zwinger/PROXY_CA_KEY` of versions before the in-memory CA. It is never imported or used. |
| macOS did not give access | You canceled the prompt, or the Keychain is locked. The item stays. |
| Kept | The item stayed in the Keychain. The report gives the reason. |
| Not in the Keychain | The item was already gone when checked. |
| Removed from the Keychain | Cleanup removed the selected item. |
| Failed | The vault could not be written or saved to disk, or more than one item has this name. The item stays. |

The import does not attach a value to the credential library, and it does not change a rule. A value of a cage that
no longer has the credential is not imported, so it stays in the Keychain and does not go at the next app start.

After an import, **Other old items** lists the items that the vault does not hold with the same value, for example the
old proxy CA key or a value of a deleted cage. If the Keychain has only the old proxy CA key, run **Import from
Keychain** to get this list. Select each item, then confirm. This deletes the only copy, and it cannot be undone. An
item that changed since import stays if the final check detects the change. macOS checks and deletes in separate calls.
Another app can still edit the item during that short interval. A macOS prompt during removal can delay other credential edits.

A kept item stays in the Keychain. To get its value, open **Keychain Access**, search for `zwinger`, open the item with
the account name from the report and select **Show password**. Then enter the value in the cage or in the credential
library.

The import receipt `legacy-keychain.json` in the app data folder keeps only the account names, an item revision from
the Keychain and the result of each item. It never keeps a value. If zwinger cannot read the receipt, or it is from a
newer version, zwinger does not change it and does not import or remove an item. Move the file away, then import again.

Starting another import clears earlier cleanup approvals. If you quit during a cleanup, run **Import from Keychain**
again to review any items that remain.

:::caution
Do not give a credential to a host that sends requests back, for example `httpbin.org`, `postman-echo.com` or an
`echo.*` host. The app shows a warning.
:::

The proxy uses its own certificate authority (CA) for HTTPS. Each app start makes a new CA. Its private key stays
only in the memory of the app: no file and no Keychain item holds it. The app writes only the CA certificate to disk,
and logs its SHA-256 fingerprint once (`proxy CA sha256 …`). Each cage start puts the current CA in the trust store
of the cage and removes the CA of an earlier app start.

**Settings > About** shows the SHA-256 fingerprint of the current CA. To compare it with the CA in a cage, run this
command in the cage. It uses only Node, so it works in the default image (`node:22-slim`):

```sh
node -e "const fs = require('node:fs'), { X509Certificate } = require('node:crypto'); console.log(new X509Certificate(fs.readFileSync('/opt/zwinger/ca/zwinger-ca.pem')).fingerprint256)"
```

If openssl is installed, this command gives the same fingerprint after `sha256 Fingerprint=`:

```sh
openssl x509 -noout -fingerprint -sha256 -in /opt/zwinger/ca/zwinger-ca.pem
```

The cage start writes the new bundle `/etc/ssl/certs/ca-certificates.crt` to a temp file next to it, and then replaces
the bundle in one atomic step. A process that reads the bundle sees the complete old bundle or the complete new bundle.
If the bundle is a symlink, the app replaces the file at the end of the links and keeps the links. The bundle keeps its
file mode. If a step fails, the old bundle stays as it was and the cage shows a notice.

The cage start does not run `update-ca-certificates`, because Debian's version replaces a symlinked bundle with a
file. If openssl or `c_rehash` (Alpine `ca-certificates`) is installed, the cage start sets up CApath trust
automatically: it adds the CA to `/etc/ssl/certs` for clients that read only that folder, as `zwinger-ca.pem` and a hash
link, with the names that `update-ca-certificates` uses. The link of an earlier app start is removed. An image with
neither tool (for example `node:22-slim`) trusts the CA only through the bundle file.

At each app start, the app stops the VMs that the last run left running. If this stop fails, the first cage start
stops them again. While it fails, no cage starts.
