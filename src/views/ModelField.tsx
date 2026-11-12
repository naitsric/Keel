import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Field, inputClass } from "../ui/primitives";

export const MODELS = [
  { value: "fable", label: "Fable" },
  { value: "opus", label: "Opus" },
  { value: "sonnet", label: "Sonnet" },
  { value: "haiku", label: "Haiku" },
];
const CUSTOM = "__custom__";

/** A model picker, with a free field for any other model name. With `none`, no model is a choice too, under that name, and it's the empty value. */
export function ModelField({ label, hint, none, value, onChange }: { label: string; hint?: string; none?: string; value: string; onChange: (value: string) => void }) {
  const { t } = useTranslation();
  const known = MODELS.some((m) => m.value === value) || (none !== undefined && value === "");
  const [custom, setCustom] = useState(!known);
  return (
    <Field label={label} hint={hint}>
      <select
        value={custom ? CUSTOM : value}
        onChange={(e) => {
          const isCustom = e.target.value === CUSTOM;
          setCustom(isCustom);
          onChange(isCustom ? "" : e.target.value);
        }}
        className={inputClass}
      >
        {none !== undefined && <option value="">{none}</option>}
        {MODELS.map((m) => (
          <option key={m.value} value={m.value}>
            {m.label}
          </option>
        ))}
        <option value={CUSTOM}>{t("newRun.customModel")}</option>
      </select>
      {custom && <input value={value} onChange={(e) => onChange(e.target.value.trim())} placeholder={t("newRun.customModelPlaceholder")} className={`${inputClass} mt-1.5 font-mono`} />}
    </Field>
  );
}
