import { Component, type ReactNode } from 'react';
import { Section } from './ui';
import { alertText } from './screenStyles';

/**
 * One part of a page failing to draw (a value the app did not expect) must not blank the rest of the page: it says so, and the rest stays.
 * The same guard as the one in the Permissions page, for the parts of the Autonomy and Scoreboard screens.
 */
export class PartGuard extends Component<{ name: string; children: ReactNode }, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(error: unknown) {
    console.error('[ui] a part of a page crashed:', this.props.name, error);
  }

  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <Section title={this.props.name}>
        <div role="alert" style={alertText}>This part of the page could not be shown. The rest of the page still works.</div>
      </Section>
    );
  }
}
