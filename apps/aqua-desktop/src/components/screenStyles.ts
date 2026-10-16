// Small style objects shared by the memory, vault, skills inbox and dictation screens. They use the CSS variables of index.css, so
// the screens look like the pages that were there before.

import type { CSSProperties } from 'react';

export const page: CSSProperties = { display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 980, margin: '0 auto' };
export const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)', lineHeight: 1.5 };
export const small: CSSProperties = { padding: '3px 10px' };
export const alertText: CSSProperties = { color: 'var(--state-problem)', fontSize: 13, overflowWrap: 'anywhere' };
export const noticeText: CSSProperties = { color: 'var(--state-ok)', fontSize: 13, overflowWrap: 'anywhere' };
export const row: CSSProperties = { display: 'flex', gap: 12, alignItems: 'center', justifyContent: 'space-between', flexWrap: 'wrap', padding: '10px 0' };
export const rowBorder = '1px solid var(--glass-border)';
export const input: CSSProperties = {
  flex: 1,
  minWidth: 0,
  padding: '9px 12px',
  borderRadius: 8,
  border: '1px solid var(--glass-border)',
  background: 'rgba(0,0,0,0.3)',
  color: 'var(--text-primary)',
  outline: 'none',
  fontFamily: 'inherit',
  fontSize: 14,
  userSelect: 'text',
};
/** A box of text the person reads and may copy from (the body stays text, never markup). */
export const textBox: CSSProperties = {
  ...input,
  width: '100%',
  minHeight: 120,
  resize: 'vertical',
  lineHeight: 1.55,
  whiteSpace: 'pre-wrap',
  overflowWrap: 'anywhere',
};
export const mark: CSSProperties = { fontSize: 11, padding: '2px 8px', borderRadius: 999, border: '1px solid var(--state-attention)', color: 'var(--state-attention)', whiteSpace: 'nowrap', cursor: 'help' };
