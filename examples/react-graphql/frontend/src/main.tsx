import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles.css";

// The Autumn-rendered shell (`index` in ../src/main.rs) and the dev-only
// `index.html` both provide `#root`; mounting is the only thing this file does.
const root = document.getElementById("root");
if (!root) {
  throw new Error("no #root element in the page shell");
}
createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
