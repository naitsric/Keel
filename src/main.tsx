import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "./styles/app.css";
import "./theme";
import "./i18n";

import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { startApp } from "./lib/store";

if (!import.meta.env.DEV) {
  // A released app must not behave like a web page: no reload and no zoom.
  window.addEventListener(
    "keydown",
    (e) => {
      const reload = e.metaKey && e.key.toLowerCase() === "r";
      const zoom = e.metaKey && ["=", "+", "-", "0"].includes(e.key);
      if (reload || zoom) e.preventDefault();
    },
    true,
  );
}

void startApp();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
