import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { Recover } from "./shared/Recover";
import { applyCachedTheme } from "./features/appearance/themes";
import { revealThemedWindow } from "./features/appearance/reveal";
import "@driif/motiv/styles.css";
import "./styles/tokens.css";
import "./styles/app.css";

applyCachedTheme();
void revealThemedWindow();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Recover>
      <App />
    </Recover>
  </React.StrictMode>,
);
