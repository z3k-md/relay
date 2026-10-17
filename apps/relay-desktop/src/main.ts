import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { createApp } from "vue";
import App from "./App.vue";
import DropPopup from "./DropPopup.vue";
import ExplorerApp from "./explorer/ExplorerApp.vue";
import "./style.css";

// The drop popup (D50) and the explorer (D51) are their own windows on the
// same page.
const popup = new URLSearchParams(window.location.search).get("popup");
const isExplorer = getCurrentWebviewWindow().label === "explorer";
createApp(isExplorer ? ExplorerApp : popup === "drop" ? DropPopup : App).mount("#app");
