import { createApp } from "vue";
import App from "./App.vue";
import DropPopup from "./DropPopup.vue";
import "./style.css";

// The drop popup (D50) is its own window on the same page.
const popup = new URLSearchParams(window.location.search).get("popup");
createApp(popup === "drop" ? DropPopup : App).mount("#app");
