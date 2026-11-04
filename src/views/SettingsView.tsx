import { useTranslation } from "react-i18next";
import type { Language } from "../i18n";
import { useApp } from "../lib/store";
import { Card, Segmented } from "../ui/primitives";
import { TopBar } from "./TopBar";

export function SettingsView() {
  const { t, i18n } = useTranslation();
  const setLanguage = useApp((s) => s.setLanguage);
  return (
    <>
      <TopBar back />
      <main className="flex-1 overflow-y-auto p-5">
        <h1 className="text-[18px] font-semibold">{t("settings.title")}</h1>
        <Card className="mt-4 max-w-xl p-4">
          <p className="text-[13px] font-medium">{t("settings.language")}</p>
          <p className="mt-0.5 mb-3 text-[12px] text-muted">{t("settings.languageHint")}</p>
          <Segmented<Language>
            label={t("settings.language")}
            value={i18n.language === "es" ? "es" : "en"}
            onChange={(language) => void setLanguage(language)}
            options={[
              { value: "en", label: t("settings.languages.en") },
              { value: "es", label: t("settings.languages.es") },
            ]}
          />
        </Card>
      </main>
    </>
  );
}
