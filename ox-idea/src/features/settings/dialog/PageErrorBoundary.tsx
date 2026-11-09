import { Component, type ReactNode } from "react";
import { EmptyState } from "@/ui";

interface Props {
  pageId: string;
  children: ReactNode;
}

/** Keeps a failing settings page (bad contribution or unexpected values) from taking down the dialog. */
export class PageErrorBoundary extends Component<Props, { error: Error | null; pageId: string }> {
  state = { error: null as Error | null, pageId: this.props.pageId };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  static getDerivedStateFromProps(props: Props, state: { error: Error | null; pageId: string }) {
    return props.pageId !== state.pageId ? { error: null, pageId: props.pageId } : null;
  }

  componentDidCatch(error: Error) {
    console.error(`settings page ${this.props.pageId} failed`, error);
  }

  render() {
    if (this.state.error)
      return (
        <EmptyState icon="warning" title="This page could not be displayed">
          {this.state.error.message}
        </EmptyState>
      );
    return this.props.children;
  }
}
