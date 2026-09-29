import { Button, Header, Input, Selection, TextInput } from "@/components";
import { ProviderKind, TYPE_PROVIDER } from "@/types";
import curl2Json, { ResultJSON } from "@bany/curl-to-json";
import { KeyIcon, TrashIcon } from "lucide-react";
import { useEffect, useState } from "react";
import {
  setProviderSecret,
  deleteProviderSecret,
  listProviderSecretNames,
} from "@/lib";

type Selected = { provider: string; variables: Record<string, string> };

export interface ProviderView {
  kind: ProviderKind;
  providers: TYPE_PROVIDER[];
  selected: Selected;
  onSelect: (selected: Selected) => void;
  variables: { key: string; value: string }[];
}

export const Providers = ({
  kind,
  providers,
  selected,
  onSelect,
  variables,
}: ProviderView) => {
  const label = kind === "ai" ? "AI" : "STT";
  const [localSelectedProvider, setLocalSelectedProvider] =
    useState<ResultJSON | null>(null);
  // The secret value lives in the OS keychain and is never read back into JS.
  const [apiKeyInput, setApiKeyInput] = useState("");
  const [apiKeyStored, setApiKeyStored] = useState(false);
  const [keyError, setKeyError] = useState<string | null>(null);

  useEffect(() => {
    if (selected?.provider) {
      const provider = providers?.find((p) => p?.id === selected?.provider);
      if (provider) {
        setLocalSelectedProvider(curl2Json(provider?.curl) as ResultJSON);
      }
    }
  }, [selected?.provider]);

  useEffect(() => {
    setApiKeyInput("");
    setApiKeyStored(false);
    setKeyError(null);
    if (!selected?.provider) return;
    listProviderSecretNames(kind, selected.provider)
      .then((names) =>
        setApiKeyStored(names.map((n) => n.toUpperCase()).includes("API_KEY"))
      )
      .catch((e) => setKeyError(String(e)));
  }, [kind, selected?.provider]);

  const findKeyAndValue = (key: string) => {
    return variables?.find((v) => v?.key === key);
  };

  const submitApiKey = () => {
    if (!selected?.provider || !apiKeyInput.trim()) return;
    setProviderSecret(kind, selected.provider, "API_KEY", apiKeyInput.trim())
      .then(() => {
        setApiKeyInput("");
        setApiKeyStored(true);
        setKeyError(null);
      })
      .catch((e) => setKeyError(String(e)));
  };

  const clearApiKey = () => {
    if (!selected?.provider) return;
    deleteProviderSecret(kind, selected.provider, "API_KEY")
      .then(() => {
        setApiKeyInput("");
        setApiKeyStored(false);
        setKeyError(null);
      })
      .catch((e) => setKeyError(String(e)));
  };

  const providerName = providers?.find((p) => p?.id === selected?.provider)
    ?.isCustom
    ? "Custom Provider"
    : selected?.provider;

  return (
    <div className="space-y-3">
      <div className="space-y-2">
        <Header
          title={`Select ${label} Provider`}
          description={`Select your preferred ${label} service provider or custom providers to get started.`}
        />
        <Selection
          selected={selected?.provider}
          options={providers?.map((provider) => {
            const json = curl2Json(provider?.curl);
            return {
              label: provider?.isCustom
                ? json?.url || "Custom Provider"
                : provider?.id || "Custom Provider",
              value: provider?.id || "Custom Provider",
              isCustom: provider?.isCustom,
            };
          })}
          placeholder={`Choose your ${label} provider`}
          onChange={(value) => {
            onSelect({ provider: value, variables: {} });
          }}
        />
      </div>

      {localSelectedProvider ? (
        <Header
          title={`Method: ${
            localSelectedProvider?.method || "Invalid"
          }, Endpoint: ${localSelectedProvider?.url || "Invalid"}`}
          description={`If you want to use different url or method, you can always create a custom provider.`}
        />
      ) : null}

      {findKeyAndValue("api_key") ? (
        <div className="space-y-2">
          <Header
            title="API Key"
            description={`Enter your ${providerName} API key to authenticate and access ${label} models. Your key is stored in the system keychain and never shared.`}
          />

          <div className="space-y-2">
            <div className="flex gap-2">
              <Input
                type="password"
                placeholder={apiKeyStored ? "•••••••• (stored)" : "**********"}
                value={apiKeyInput}
                onChange={(value) => {
                  setApiKeyInput(
                    typeof value === "string" ? value : value.target.value
                  );
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") submitApiKey();
                }}
                disabled={false}
                className="flex-1 h-11 border-1 border-input/50 focus:border-primary/50 transition-colors"
              />
              {!apiKeyStored ? (
                <Button
                  onClick={submitApiKey}
                  disabled={!apiKeyInput.trim()}
                  size="icon"
                  className="shrink-0 h-11 w-11"
                  title="Submit API Key"
                >
                  <KeyIcon className="h-4 w-4" />
                </Button>
              ) : (
                <Button
                  onClick={clearApiKey}
                  size="icon"
                  variant="destructive"
                  className="shrink-0 h-11 w-11"
                  title="Remove API Key"
                >
                  <TrashIcon className="h-4 w-4" />
                </Button>
              )}
            </div>
            {keyError && (
              <p className="text-xs text-red-500">Keychain error: {keyError}</p>
            )}
          </div>
        </div>
      ) : null}

      <div className="space-y-4 mt-2">
        {variables
          .filter(
            (variable) => variable.key !== findKeyAndValue("api_key")?.key
          )
          .map((variable) => (
            <div className="space-y-1" key={variable?.key}>
              <Header
                title={variable?.value || ""}
                description={`add your preferred ${variable?.key?.replace(
                  /_/g,
                  " "
                )} for ${providerName}`}
              />
              <TextInput
                placeholder={`Enter ${providerName} ${
                  variable?.key?.replace(/_/g, " ") || "value"
                }`}
                value={selected?.variables?.[variable.key] || ""}
                onChange={(value) => {
                  if (!variable?.key || !selected) return;
                  onSelect({
                    ...selected,
                    variables: { ...selected.variables, [variable.key]: value },
                  });
                }}
              />
            </div>
          ))}
      </div>
    </div>
  );
};
