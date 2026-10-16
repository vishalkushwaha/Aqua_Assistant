import React from 'react';

interface Props {
  children: React.ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * Without this, one bad value from the core (an `undefined.map`, a missing field) makes React unmount the whole page, and the
 * window stays blank until it is reloaded by hand. For the always-on orb a blank window means no way to talk to Aqua.
 * It shows what went wrong and a button to reload; everything under it is left as it was written.
 */
export class ErrorBoundary extends React.Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: unknown): State {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidCatch(error: unknown, info: React.ErrorInfo) {
    console.error('[ui] a component crashed:', error, info.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div
        role="alert"
        style={{
          margin: 8,
          padding: 16,
          maxWidth: 520,
          borderRadius: 12,
          background: '#0b0f16',
          border: '1px solid #7f1d1d',
          color: '#e5e7eb',
          fontFamily: 'system-ui',
          fontSize: 13,
        }}
      >
        <strong style={{ color: '#f87171' }}>This window hit a problem.</strong>
        <p style={{ margin: '8px 0', color: '#9ca3af', wordBreak: 'break-word' }}>{error.message}</p>
        <button onClick={() => window.location.reload()} style={{ padding: '6px 14px', borderRadius: 8, cursor: 'pointer' }}>
          Reload this window
        </button>
      </div>
    );
  }
}
