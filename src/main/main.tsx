import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "../styles.css";
import { listen } from "../lib/ipc";
import type { Navigate } from "../lib/types";
import { connectAppStore, useApp } from "../stores/app";
import { connectCodeStore } from "../lib/code";
import { connectRoomsStore } from "../lib/rooms";
import { connectChatStore } from "../stores/chat";
import { App } from "./App";

connectAppStore();
connectChatStore();
connectCodeStore();
connectRoomsStore();
listen<Navigate>("lg:navigate", (nav) => useApp.getState().navigate(nav));

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
