import * as Dialog from "@radix-ui/react-dialog";
import type { ComponentProps, KeyboardEvent, ReactNode } from "react";
import { twMerge } from "tailwind-merge";
import { useKeymapHold } from "../commands/keymapHold";
import { formatKeybinding } from "../commands/keys";
import type { CheckedCriterion, ShownState } from "../lib/api";

/** Joins class names; on conflicting Tailwind utilities the later one wins. */
export const cx = (...classes: (string | false | null | undefined)[]) => twMerge(classes.filter(Boolean).join(" "));

type ButtonVariant = "primary" | "secondary" | "ghost" | "danger";

const variants: Record<ButtonVariant, string> = {
  primary: "bg-accent text-accent-fg hover:brightness-110 active:brightness-95",
  secondary: "bg-elevated text-fg border border-border-strong hover:bg-hover active:bg-active",
  ghost: "text-muted hover:bg-hover hover:text-fg active:bg-active",
  danger: "text-danger border border-border-strong hover:bg-hover active:bg-active",
};

export function Button({ variant = "secondary", className, ...props }: ComponentProps<"button"> & { variant?: ButtonVariant }) {
  return (
    <button
      type="button"
      className={cx(
        "inline-flex h-7 items-center gap-1.5 rounded-md px-2.5 text-[12px] font-medium whitespace-nowrap transition-colors",
        "disabled:pointer-events-none disabled:opacity-40",
        variants[variant],
        className,
      )}
      {...props}
    />
  );
}

const STATE_TONES: Record<ShownState, string> = {
  queued: "text-muted",
  preparing: "text-accent",
  running: "text-accent",
  stalled: "text-warning",
  succeeded: "text-success",
  failed: "text-danger",
  stopped: "text-muted",
  interrupted: "text-warning",
  blocked: "text-warning",
  waitingApproval: "text-warning",
  waitingAnswers: "text-warning",
  watching: "text-accent",
  rejected: "text-muted",
  returned: "text-muted",
  postponed: "text-muted",
  // Keel had it fixed straight away: nothing for the person to look into.
  corrected: "text-muted",
  afterSetup: "text-muted",
  skipped: "text-subtle",
  abandoned: "text-subtle",
};

const STATE_DOTS: Record<ShownState, string> = {
  queued: "bg-subtle",
  preparing: "bg-accent animate-pulse",
  running: "bg-accent animate-pulse",
  stalled: "bg-warning animate-pulse",
  succeeded: "bg-success",
  failed: "bg-danger",
  stopped: "bg-subtle",
  interrupted: "bg-warning",
  blocked: "bg-warning",
  waitingApproval: "bg-warning animate-pulse",
  waitingAnswers: "bg-warning animate-pulse",
  watching: "bg-accent",
  rejected: "bg-subtle",
  returned: "bg-subtle",
  postponed: "bg-subtle",
  corrected: "bg-subtle",
  afterSetup: "bg-subtle",
  skipped: "bg-subtle",
  abandoned: "bg-subtle",
};

/** The tone of what a verifier found of one thing that had to be true, wherever it shows. */
export const VERIFY_TONES: Record<CheckedCriterion["result"], string> = {
  met: "text-success",
  unmet: "font-medium text-warning",
  unverified: "text-muted",
};

export function StateBadge({ state, label }: { state: ShownState; label: string }) {
  return (
    <span className={cx("inline-flex items-center gap-1.5 text-[12px] font-medium whitespace-nowrap", STATE_TONES[state])}>
      <span className={cx("inline-block size-1.5 shrink-0 rounded-full", STATE_DOTS[state])} />
      {label}
    </span>
  );
}

export function Card({ className, ...props }: ComponentProps<"section">) {
  return <section className={cx("rounded-lg border border-border bg-panel", className)} {...props} />;
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1 block text-[12px] font-medium text-muted">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-[11.5px] text-subtle">{hint}</span>}
    </label>
  );
}

export const inputClass =
  "w-full rounded-md border border-border-strong bg-inset px-2 py-1.5 text-[12.5px] text-fg outline-none placeholder:text-subtle focus:border-accent";

export interface SegmentOption<T extends string> {
  value: T;
  label: ReactNode;
}

/**
 * One choice out of a few, as a row of segments. With `arrows`, ← → move between them. Only ask for
 * it where changing one just shows something else: elsewhere this is a setting, like MCP on or off.
 */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
  arrows,
}: {
  value: T;
  options: readonly SegmentOption<T>[];
  onChange: (value: T) => void;
  label: string;
  arrows?: boolean;
}) {
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!step || e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return;
    e.preventDefault();
    const next = (options.findIndex((option) => option.value === value) + step + options.length) % options.length;
    onChange(options[next].value);
    (e.currentTarget.children[next] as HTMLElement).focus();
  };
  return (
    <div role="radiogroup" aria-label={label} onKeyDown={arrows ? onKeyDown : undefined} className="inline-flex items-center rounded-md bg-inset p-0.5 text-[12px]">
      {options.map((option) => {
        const selected = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={selected}
            tabIndex={arrows && !selected ? -1 : undefined}
            onClick={() => onChange(option.value)}
            className={cx("h-6 rounded px-2.5 font-medium transition-colors", selected ? "bg-elevated text-fg shadow-sm" : "text-muted hover:text-fg")}
          >
            {option.label}
          </button>
        );
      })}
    </div>
  );
}

export function Kbd({ binding, children }: { binding?: string; children?: ReactNode }) {
  return <kbd className="rounded border border-border bg-inset px-1.5 py-px font-sans text-[11px] tracking-wider text-subtle">{binding ? formatKeybinding(binding) : children}</kbd>;
}

export function Modal({
  open,
  onOpenChange,
  title,
  description,
  children,
  className,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: string;
  children: ReactNode;
  className?: string;
}) {
  useKeymapHold(open);
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <Dialog.Content
          aria-describedby={description ? undefined : ""}
          className={cx(
            "fixed top-1/2 left-1/2 z-50 max-h-[85vh] w-[520px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 overflow-y-auto rounded-lg border border-border-strong bg-panel p-5 shadow-2xl",
            className,
          )}
        >
          <Dialog.Title className="text-[15px] font-semibold">{title}</Dialog.Title>
          {description && <Dialog.Description className="mt-1.5 text-[12.5px] leading-relaxed text-muted">{description}</Dialog.Description>}
          <div className="mt-4">{children}</div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
