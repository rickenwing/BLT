import { FormEvent, useState } from "react";
import { api } from "../api";

export default function Login({
  needsSetup,
  onAuthed,
}: {
  needsSetup: boolean;
  onAuthed: () => void;
}) {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    if (needsSetup && password !== confirm) {
      setError("Passwords don't match");
      return;
    }
    setBusy(true);
    try {
      await api.post(needsSetup ? "/api/setup" : "/api/login", { password });
      onAuthed();
    } catch (err) {
      setError(err instanceof Error ? err.message : "failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="login-wrap">
      <form className="login-box" onSubmit={submit}>
        <h1>
          <span style={{ color: "var(--accent)" }}>BLT</span> Admin
        </h1>
        <p className="dim" style={{ textAlign: "center" }}>
          {needsSetup
            ? "First run — set the admin password."
            : "Enter the admin password."}
        </p>
        <label className="field">
          <span>Password</span>
          <input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoFocus
          />
        </label>
        {needsSetup && (
          <label className="field">
            <span>Confirm password</span>
            <input
              type="password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
            />
          </label>
        )}
        {error && <div className="error-text">{error}</div>}
        <button className="primary" style={{ width: "100%" }} disabled={busy}>
          {needsSetup ? "Set password & enter" : "Log in"}
        </button>
      </form>
    </div>
  );
}
