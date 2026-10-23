---
title: Updates
description: How zwinger updates itself.
---

zwinger updates itself from the [GitHub releases](https://github.com/driif/zwinger/releases). Each update has a
signature. The app checks the signature and does not install an update with a bad signature.

## Check for updates

- By default, zwinger checks for updates when it starts. To change this, go to **Settings > Updates** and set
  **Check for updates when zwinger starts**.
- To check now, go to **Settings > Updates** and select **Check now**.

## Install an update

When an update is available, zwinger shows the version. Select **Restart to update**.

If a cage runs, zwinger asks first, because the restart ends the VMs and the work of their agents. Select **Stop cages
and restart** to continue. If a cage does not stop, the update applies at the next start.

After an update, you do not have to open the app with **Open Anyway** again.
