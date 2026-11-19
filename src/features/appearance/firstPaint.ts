import "@driif/motiv/styles.css";
import "../../styles/tokens.css";
import "../../styles/app.css";
import { applyCachedTheme, applyThemeTokens } from "./themeCache.ts";
import { BUILT_INS } from "./builtIns.ts";
import { revealThemedWindow } from "./reveal.ts";

try {
  applyCachedTheme();
} catch {
  try {
    applyThemeTokens(BUILT_INS[0]);
  } catch {
    // A failed theme operation must not leave the native window hidden.
  }
}
void revealThemedWindow();
