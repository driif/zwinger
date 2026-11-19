import { revealMainWindow } from "../../shared/api.ts";
import { DEFAULT_BACKGROUND, readAppliedBackgroundColor } from "./themeCache.ts";

function linkSettled(doc: Document, link: HTMLLinkElement) {
  return new Promise<void>((resolve) => {
    let probe: HTMLLinkElement | undefined;
    let linkDone = false;
    let probeLoaded = false;
    const listen = (target: HTMLLinkElement, onLoad: () => void, onError: () => void) => {
      target.addEventListener("load", onLoad, { once: true });
      target.addEventListener("error", onError, { once: true });
    };
    const linkSettledNow = () => {
      linkDone = true;
      // A loaded probe keeps its rules applied until the original link settles.
      if (probeLoaded) probe?.remove();
      resolve();
    };
    listen(link, linkSettledNow, linkSettledNow);
    if (link.sheet) return resolve();
    // The link can have failed before the listeners were attached. A probe with the same
    // request fails again at once, or waits with a pending sheet.
    probe = doc.createElement("link");
    listen(probe, () => {
      probeLoaded = true;
      if (linkDone) probe?.remove();
      resolve();
    }, () => {
      probe?.remove();
      resolve();
    });
    probe.rel = "stylesheet";
    probe.crossOrigin = link.crossOrigin;
    probe.href = link.href;
    doc.head.append(probe);
  });
}

/** Resolves when every stylesheet link in the head has loaded or failed. */
export async function stylesheetsSettled(doc: Document) {
  const links = [...doc.head.querySelectorAll<HTMLLinkElement>('link[rel="stylesheet"]')];
  await Promise.all(links.filter((link) => !link.sheet).map((link) => linkSettled(doc, link)));
}

/** Shows the hidden native window in the applied theme after the stylesheets apply. */
export async function revealThemedWindow() {
  try {
    await stylesheetsSettled(document);
  } catch {
    // A failed wait must not leave the native window hidden.
  }
  let background = DEFAULT_BACKGROUND;
  try {
    background = readAppliedBackgroundColor();
  } catch {
    // The default dark background is already selected.
  }
  await revealMainWindow(background).catch(() => {});
}
