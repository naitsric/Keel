import { Check, Copy } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type McpClient, type McpOverview } from "../lib/api";
import { formatRelative } from "../lib/format";
import { errorText } from "../lib/reason";
import { useApp } from "../lib/store";
import { Button, Card, cx, Field, inputClass, Segmented } from "../ui/primitives";

/** While the server is on, so the person sees calls arrive. Tool calls don't announce themselves. */
const REFRESH_MS = 3000;
const CLAUDE_CLIENT = "Claude Code";

function ClientRow({ client, onChanged, onError }: { client: McpClient; onChanged: () => void; onError: (message: string) => void }) {
  const { t } = useTranslation();
  const act = (action: Promise<void>) => action.then(onChanged).catch((e) => onError(errorText(t, e)));
  const revoked = client.revokedAt !== null;
  return (
    <li className={cx("flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-border px-3 py-2 first:border-t-0", revoked && "opacity-60")}>
      <div className="min-w-0 flex-1">
        <p className="truncate text-[12.5px] font-medium">{client.name}</p>
        <p className="font-mono text-[11px] text-subtle">
          {client.tokenPrefix}…
          <span className="ml-2 font-sans">
            {revoked ? t("mcp.revoked") : client.lastSeenAt ? t("mcp.lastSeen", { when: formatRelative(client.lastSeenAt) }) : t("mcp.neverSeen")}
          </span>
        </p>
      </div>
      {!revoked && (
        <>
          <label className="flex items-center gap-1.5 text-[12px] text-muted">
            <input type="checkbox" checked={client.canRun} onChange={(e) => void act(api.mcpClientSetCanRun(client.id, e.target.checked))} />
            {t("mcp.canRun")}
          </label>
          <Button variant="ghost" onClick={() => void act(api.mcpClientRevoke(client.id))}>
            {t("mcp.revoke")}
          </Button>
        </>
      )}
    </li>
  );
}

function NewToken({ token, url }: { token: string; url: string | null }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const text = `URL:    ${url ?? ""}\nHeader: Authorization: Bearer ${token}`;
  const copy = async () => {
    await navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };
  return (
    <div className="mt-2 rounded-md border border-border-strong bg-inset p-3">
      <p className="text-[12px] text-warning">{t("mcp.tokenOnce")}</p>
      <pre className="selectable mt-2 overflow-x-auto font-mono text-[11.5px] leading-relaxed">{text}</pre>
      <Button className="mt-2" onClick={() => void copy()}>
        {copied ? <Check size={13} /> : <Copy size={13} />}
        {t(copied ? "mcp.copied" : "mcp.copy")}
      </Button>
    </div>
  );
}

export function McpCard() {
  const { t } = useTranslation();
  const navigate = useApp((s) => s.navigate);
  const [overview, setOverview] = useState<McpOverview | null>(null);
  const [port, setPort] = useState("");
  const [canRun, setCanRun] = useState(true);
  const [name, setName] = useState("");
  const [token, setToken] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    api
      .mcpOverview()
      .then(setOverview)
      .catch((e) => setError(errorText(t, e)));
  }, [t]);

  useEffect(load, [load]);

  const running = overview?.status.running ?? false;
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(load, REFRESH_MS);
    return () => clearInterval(timer);
  }, [running, load]);

  const savedPort = overview?.settings.port;
  useEffect(() => {
    if (savedPort !== undefined) setPort(String(savedPort));
  }, [savedPort]);

  if (!overview) return null;
  const { settings, status, clients, calls } = overview;
  const portNumber = Number(port);
  const validPort = Number.isInteger(portNumber) && portNumber >= 1024 && portNumber <= 65535;

  const run = async <T,>(action: () => Promise<T>, done: (result: T) => void) => {
    setBusy(true);
    setError(null);
    try {
      done(await action());
    } catch (e) {
      setError(errorText(t, e));
    } finally {
      setBusy(false);
    }
  };

  const save = (enabled: boolean, nextPort: number) =>
    void run(
      () => api.mcpSettingsSave({ enabled, port: nextPort }),
      (next) => {
        setOverview(next);
        if (!enabled) setConnected(false);
      },
    );

  const connect = () =>
    void run(
      () => api.mcpConnectClaude(canRun),
      (next) => {
        setOverview(next);
        setConnected(true);
      },
    );

  const create = () =>
    void run(
      () => api.mcpClientCreate(name, canRun),
      (created) => {
        setToken(created.token);
        setName("");
        load();
      },
    );

  const claudeIsConnected = clients.some((client) => client.name === CLAUDE_CLIENT && !client.revokedAt);

  return (
    <Card className="p-4">
      <p className="text-[13px] font-medium">{t("mcp.title")}</p>
      <p className="mt-0.5 text-[12px] leading-snug text-muted">{t("mcp.hint")}</p>
      <p className="mt-1 text-[12px] leading-snug text-muted">{t("mcp.limits")}</p>
      <div className="mt-3 flex flex-wrap items-center gap-3">
        <Segmented<"off" | "on">
          label={t("mcp.title")}
          value={settings.enabled ? "on" : "off"}
          onChange={(value) => save(value === "on", settings.port)}
          options={[
            { value: "off", label: t("mcp.off") },
            { value: "on", label: t("mcp.on") },
          ]}
        />
        {status.running && <span className="selectable font-mono text-[11.5px] text-subtle">{status.url}</span>}
      </div>

      {status.error && (
        <p role="alert" className="mt-2 text-[12.5px] text-warning">
          {status.error.code === "portInUse" ? t("mcp.portInUse", { port: status.error.port }) : t("mcp.listenFailed", { port: status.error.port, message: status.error.message })}
        </p>
      )}

      {(settings.enabled || status.error) && (
        <div className="mt-3 flex items-end gap-2">
          <div className="w-28">
            <Field label={t("mcp.port")}>
              <input inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} className={`${inputClass} font-mono`} />
            </Field>
          </div>
          <Button disabled={busy || !validPort || (portNumber === settings.port && !status.error)} onClick={() => save(settings.enabled, portNumber)}>
            {t("mcp.applyPort")}
          </Button>
        </div>
      )}

      {status.running && (
        <>
          <div className="mt-4 flex flex-wrap items-center gap-3">
            <Button variant="primary" disabled={busy} onClick={connect}>
              {t(claudeIsConnected ? "mcp.reconnectClaude" : "mcp.connectClaude")}
            </Button>
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <input type="checkbox" checked={canRun} onChange={(e) => setCanRun(e.target.checked)} />
              {t("mcp.canRun")}
            </label>
          </div>
          {connected && <p className="mt-2 text-[12px] leading-snug text-success">{t("mcp.connected")}</p>}

          <p className="mt-4 text-[12.5px] font-semibold">{t("mcp.clients")}</p>
          {clients.length === 0 ? (
            <p className="mt-1 text-[12px] text-subtle">{t("mcp.noClients")}</p>
          ) : (
            <ul className="mt-1.5 rounded-md border border-border">
              {clients.map((client) => (
                <ClientRow key={client.id} client={client} onChanged={load} onError={setError} />
              ))}
            </ul>
          )}

          <div className="mt-3 flex items-end gap-2">
            <div className="w-56">
              <Field label={t("mcp.otherClient")}>
                <input value={name} onChange={(e) => setName(e.target.value)} placeholder={t("mcp.otherClientPlaceholder")} className={inputClass} />
              </Field>
            </div>
            <Button disabled={busy || name.trim() === ""} onClick={create}>
              {t("mcp.createClient")}
            </Button>
          </div>
          {token && <NewToken token={token} url={status.url} />}

          <p className="mt-4 text-[12.5px] font-semibold">{t("mcp.activity")}</p>
          {calls.length === 0 ? (
            <p className="mt-1 text-[12px] text-subtle">{t("mcp.noActivity")}</p>
          ) : (
            <ul className="mt-1.5 max-h-56 overflow-y-auto rounded-md border border-border">
              {calls.map((call) => (
                <li key={call.seq} title={call.args} className="flex items-baseline gap-2 border-t border-border px-3 py-1.5 text-[12px] first:border-t-0">
                  <span className={cx("font-mono text-[11.5px]", call.ok ? "text-fg" : "text-warning")}>{call.tool}</span>
                  <span className="min-w-0 flex-1 truncate text-subtle">{call.ok ? call.clientName : call.error}</span>
                  {call.runId && (
                    <button type="button" className="shrink-0 text-accent hover:underline" onClick={() => navigate({ name: "run", runId: call.runId as string })}>
                      {t("mcp.openRun")}
                    </button>
                  )}
                  <span className="shrink-0 text-[11px] text-subtle">{formatRelative(call.at)}</span>
                </li>
              ))}
            </ul>
          )}
        </>
      )}

      {error && (
        <p role="alert" className="mt-2 text-[12px] text-danger">
          {error}
        </p>
      )}
    </Card>
  );
}
