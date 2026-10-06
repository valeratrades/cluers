import { Header } from "@/components";
import { useApp } from "@/contexts";
import { ProviderKind, UseSettingsReturn } from "@/types";
import { Providers, ProviderView } from "./Providers";
import { CustomProviders } from "./CustomProviders";

export const ProviderSection = ({
  kind,
  ...settings
}: UseSettingsReturn & { kind: ProviderKind }) => {
  const { sttMigrationError } = useApp();
  const view: ProviderView =
    kind === "ai"
      ? {
          kind,
          providers: settings.allAiProviders,
          selected: settings.selectedAIProvider,
          onSelect: settings.onSetSelectedAIProvider,
          variables: settings.variables,
        }
      : {
          kind,
          providers: settings.allSttProviders,
          selected: settings.selectedSttProvider,
          onSelect: settings.onSetSelectedSttProvider,
          variables: settings.sttVariables,
        };
  const label = kind === "ai" ? "AI" : "STT";

  return (
    <div id={`${kind}-providers`} className="space-y-3">
      <Header
        title={`${label} Providers`}
        description={`Select your preferred ${label} service provider to get started.`}
        isMainTitle
      />
      {kind === "stt" && sttMigrationError && (
        <p className="text-xs text-red-500">
          Moving the saved STT API key into the system keychain failed; it is
          still stored in plain text and will be retried on next start:{" "}
          {sttMigrationError}
        </p>
      )}
      <CustomProviders kind={kind} providers={view.providers} />
      <Providers {...view} />
    </div>
  );
};
