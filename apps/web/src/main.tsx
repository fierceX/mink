// Web entry: theme sheets first (they carry every --dsw-* token the components
// read), then the workbench shell into #root.
import "./theme/styles/brand-font.css";
import "./theme/styles/base.css";
import "./theme/styles/corner-shape.css";
import "./theme/styles/design-platform.css";
import "./theme/styles/focus.css";
import "./theme/styles/onboarding.css";
import "./theme/styles/scrollbar.css";
import "./theme/styles/gradient-shadow-text.css";
import "./theme/styles/shiki.css";
import "./theme/shell-base.css";
import "./app/app.css";
import { createRoot } from "react-dom/client";
import { App } from "./app/App.tsx";
import { applyPreferences, loadPreferences, systemPrefersDark } from "./app/settings/preferences.ts";

// Apply the stored theme and font size before the first paint, the same
// pre-mount step the source's boot injection performs.
const boot = loadPreferences();
applyPreferences(boot, boot.theme === "dark" || (boot.theme === "system" && systemPrefersDark()));

const el = document.getElementById("root");
if (el === null) throw new Error("web app: missing #root");
createRoot(el).render(<App />);
