import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { Recover } from "./shared/Recover";
import { applyCachedTheme } from "./features/appearance/themes";
import "@driif/motiv/styles.css";
import "./styles/tokens.css";
import "./styles/app.css";

applyCachedTheme();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Recover>
      <App />
    </Recover>
  </React.StrictMode>,
);
