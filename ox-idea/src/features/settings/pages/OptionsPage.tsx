import { Section, SettingNumber, SettingSelect, SettingText, SettingToggle } from "../components/Form";
import type { OptionSpec, PageSpec } from "./spec";
import styles from "./Pages.module.css";

function Option({ o }: { o: OptionSpec }) {
  switch (o.kind) {
    case "toggle":
      return <SettingToggle k={o.key} label={o.label} hint={o.hint} />;
    case "number":
      return (
        <SettingNumber
          k={o.key}
          label={o.label}
          hint={o.hint}
          min={o.min}
          max={o.max}
          step={o.step}
          integer={o.integer}
          unit={o.unit}
        />
      );
    case "select":
      return <SettingSelect k={o.key} label={o.label} hint={o.hint} options={o.options} />;
    case "text":
      return <SettingText k={o.key} label={o.label} hint={o.hint} placeholder={o.placeholder} />;
    case "custom": {
      const C = o.component;
      return <C />;
    }
  }
}

/** Renders a declarative page spec. */
export function OptionsPage({ spec }: { spec: PageSpec }) {
  const Footer = spec.footer;
  return (
    <div className={styles.options}>
      {spec.sections?.map((section, i) => (
        <Section key={section.title ?? i} title={section.title}>
          {section.options.map((o) => (
            <Option key={"key" in o ? o.key : o.label} o={o} />
          ))}
        </Section>
      ))}
      {Footer && <Footer />}
    </div>
  );
}
