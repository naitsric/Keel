import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./en.json";
import es from "./es.json";

export type Language = "en" | "es";

/** Spanish if the system is in Spanish, English otherwise. */
export function systemLanguage(): Language {
  return navigator.language.toLowerCase().startsWith("es") ? "es" : "en";
}

void i18next.use(initReactI18next).init({
  resources: { en: { translation: en }, es: { translation: es } },
  lng: systemLanguage(),
  fallbackLng: "en",
  // React already escapes. Escaping twice would show `&#39;` in commands and paths.
  interpolation: { escapeValue: false },
});

i18next.on("languageChanged", (language) => {
  document.documentElement.lang = language;
});
document.documentElement.lang = i18next.language;

export default i18next;
