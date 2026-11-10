import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Language } from "../i18n";
import { api } from "../lib/api";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Card, Segmented } from "../ui/primitives";
import { McpCard } from "./McpCard";

const AGENT_LIMITS = Array.from({ length: 10 }, (_, i) => String(i + 1));

function MaxAgentsCard() {
  const { t } = useTranslation();
  const [value, setValue] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .maxAgentsGet()
      .then((limit) => setValue(String(limit)))
      .catch((e) => setError(errorText(t, e)));
  }, [t]);

  const change = (next: string) => {
    const previous = value;
    setValue(next);
    setError(null);
    api.maxAgentsSet(Number(next)).catch((e) => {
      setValue(previous);
      setError(errorText(t, e));
    });
  };

  return (
    <Card className="p-4">
      <p className="text-[13px] font-medium">{t("settings.maxAgents")}</p>
      <p className="mt-0.5 mb-3 text-[12px] text-muted">{t("settings.maxAgentsHint")}</p>
      {value !== null && <Segmented<string> label={t("settings.maxAgents")} value={value} onChange={change} options={AGENT_LIMITS.map((limit) => ({ value: limit, label: limit }))} />}
      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
    </Card>
  );
}

export function SettingsView() {
  const { t, i18n } = useTranslation();
  const setLanguage = useApp((s) => s.setLanguage);
  return (
    <main className="flex-1 overflow-y-auto p-5">
      <div className="mx-auto w-full max-w-3xl">
        <h1 className="text-[18px] font-semibold">{t("settings.title")}</h1>
        <div className="mt-4 space-y-4">
          <Card className="p-4">
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
          <MaxAgentsCard />
          <McpCard />
        </div>
      </div>
    </main>
  );
}
