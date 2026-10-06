import {
  Card,
  Button,
  Header,
  TextInput,
  Switch,
  Textarea,
  Selection,
} from "@/components";
import { PlusIcon, SaveIcon } from "lucide-react";
import { ReactNode } from "react";
import { useCustomProviders } from "@/hooks";
import { ProviderKind, TYPE_PROVIDER } from "@/types";
import { cn } from "@/lib/utils";

const Code = ({ children }: { children: ReactNode }) => (
  <code className="bg-muted px-1 rounded text-xs">{children}</code>
);

const FORM: Record<
  ProviderKind,
  {
    label: string;
    curlPlaceholder: string;
    variables: { name: string; help: string; required?: boolean }[];
    tip: ReactNode;
    pathPlaceholder: string;
    pathNotes: string;
    streaming: boolean;
  }
> = {
  ai: {
    label: "AI",
    curlPlaceholder: `curl --location 'http://127.0.0.1:1337/v1/chat/completions' \\
--header 'Content-Type: application/json' \\
--header 'Authorization: Bearer YOUR_API_KEY or {{API_KEY}}' \\
--data '{
        "model": "your-model-name or {{MODEL}}",
        "messages": [
            {
                "role": "system",
                "content": "{{SYSTEM_PROMPT}}"
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "{{TEXT}}"
                    },
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": "data:image/jpeg;base64,{{IMAGE}}"
                        }
                    }
                ]
            }
        ]
    }'`,
    variables: [
      { name: "TEXT", help: "User's text input", required: true },
      {
        name: "IMAGE",
        help: "Base64 image data (without data:image/jpeg;base64 prefix)",
      },
      { name: "SYSTEM_PROMPT", help: "System prompt/instructions(optional)" },
    ],
    tip: (
      <>
        💡 Tip: Use the required variables (<Code>{"{{TEXT}}"}</Code>,{" "}
        <Code>{"{{SYSTEM_PROMPT}}"}</Code>) for basic functionality. Add{" "}
        <Code>{"{{IMAGE}}"}</Code> only if your provider supports image input.
      </>
    ),
    pathPlaceholder: "choices[0].message.content",
    pathNotes:
      "The path to extract content from the API response. Examples: choices[0].message.content, text, candidates[0].content.parts[0].text",
    streaming: true,
  },
  stt: {
    label: "STT",
    curlPlaceholder: `curl -X POST "https://api.openai.com/v1/audio/transcriptions" \\
      -H "Authorization: Bearer {{API_KEY}}" \\
      -F "file={{AUDIO}}" \\
      -F "model={{MODEL}}"`,
    variables: [
      {
        name: "AUDIO",
        help: "Audio file part with -F/--form, raw bytes with --data-binary, or base64 inside a -d JSON body",
        required: true,
      },
    ],
    tip: (
      <>
        💡 Tip: The <Code>{"{{AUDIO}}"}</Code> variable is essential for STT
        functionality - make sure it's properly included in your curl command.
      </>
    ),
    pathPlaceholder: "text",
    pathNotes:
      "The path to extract content from the API response. Examples: text, transcript, results[0].alternatives[0].transcript",
    streaming: false,
  },
};

interface CreateEditProviderProps {
  kind: ProviderKind;
  providers: TYPE_PROVIDER[];
  customProviderHook: ReturnType<typeof useCustomProviders>;
}

export const CreateEditProvider = ({
  kind,
  providers,
  customProviderHook,
}: CreateEditProviderProps) => {
  const form = FORM[kind];
  const {
    showForm,
    setShowForm,
    editingProvider,
    formData,
    setFormData,
    errors,
    handleSave,
    setErrors,
    handleAutoFill,
  } = customProviderHook;

  return (
    <>
      {!showForm ? (
        <Button
          onClick={() => {
            setShowForm(true);
            setErrors({});
          }}
          variant="outline"
          className="w-full h-11 border-1 border-input/50 focus:border-primary/50 transition-colors"
        >
          <PlusIcon className="h-4 w-4 mr-2" />
          Add Custom {form.label} Provider
        </Button>
      ) : (
        <Card className="p-4 border !bg-transparent border-input/50 ">
          <div className="flex justify-between items-center">
            <Header
              title={
                editingProvider
                  ? `Edit ${form.label} Provider`
                  : `Add Custom ${form.label} Provider`
              }
              description={`Create a custom ${form.label} provider to use with your ${form.label}-powered applications.`}
            />

            <div className="w-[120px]">
              <Selection
                options={providers
                  ?.filter((provider) => !provider?.isCustom)
                  .map((provider) => ({
                    label: provider?.id || `${form.label} Provider`,
                    value: provider?.id || `${form.label} Provider`,
                  }))}
                placeholder={"Auto-fill"}
                onChange={(value) => {
                  handleAutoFill(value);
                }}
              />
            </div>
          </div>

          <div className="">
            <div className="space-y-1">
              <Header
                title="Curl Command *"
                description={`The curl command to use with the ${form.label} provider.`}
              />
              <Textarea
                className={cn(
                  "h-74 font-mono text-sm",
                  errors.curl && "border-red-500"
                )}
                placeholder={form.curlPlaceholder}
                value={formData.curl}
                onChange={(e) =>
                  setFormData((prev) => ({ ...prev, curl: e.target.value }))
                }
              />
              {errors.curl && (
                <p className="text-xs text-red-500 mt-1">{errors.curl}</p>
              )}

              <div className="bg-muted/50 p-4 rounded-lg space-y-4">
                <div className="bg-card border p-3 rounded-lg">
                  <p className="text-sm font-medium text-primary mb-2">
                    💡 Important: You can add custom variables or directly
                    include your API keys/values
                  </p>
                  <p className="text-xs text-muted-foreground">
                    No need to enter variables separately when selecting the
                    provider - you can embed them directly in the curl command
                    (e.g., replace YOUR_API_KEY with your actual key or use{" "}
                    <Code>{"{{MODEL}}"}</Code> for model name).
                  </p>
                </div>

                <h4 className="text-sm font-semibold text-foreground">
                  ⚠️ Required Variables for {form.label} Providers:
                </h4>
                <div className="grid grid-cols-1 gap-3 text-sm">
                  {form.variables.map((v) => (
                    <div
                      key={v.name}
                      className="flex items-center gap-3 p-3 bg-card border rounded-lg"
                    >
                      <code className="bg-muted px-2 py-1 rounded font-mono text-xs">
                        {`{{${v.name}}}`}
                      </code>
                      <span
                        className={
                          v.required
                            ? "text-foreground font-medium"
                            : "text-muted-foreground"
                        }
                      >
                        → {v.required && "REQUIRED: "}
                        {v.help}
                      </span>
                    </div>
                  ))}
                </div>

                <div className="space-y-3">
                  <p className="text-sm text-muted-foreground">
                    <strong className="text-foreground">Quick Setup:</strong>{" "}
                    Replace <Code>YOUR_API_KEY</Code> with your actual API key
                    directly in the curl command.
                  </p>
                  <p className="text-sm text-muted-foreground">
                    <strong className="text-foreground">
                      Custom Variables:
                    </strong>{" "}
                    You can add your own variables using the same{" "}
                    <Code>{"{{VARIABLE_NAME}}"}</Code> format and they'll be
                    available for configuration when you select this provider.
                  </p>
                  <p className="text-xs text-muted-foreground italic">
                    {form.tip}
                  </p>
                </div>
              </div>
            </div>
          </div>

          {form.streaming && (
            <div className="flex justify-between items-center space-x-2">
              <Header
                title="Streaming"
                description="streaming is used to stream the response from the AI provider."
              />
              <Switch
                checked={formData.streaming}
                onCheckedChange={(checked) =>
                  setFormData((prev) => ({ ...prev, streaming: checked }))
                }
              />
            </div>
          )}
          <div className="space-y-2">
            <Header
              title="Response Content Path *"
              description="The path to extract content from the API response."
            />

            <TextInput
              placeholder={form.pathPlaceholder}
              value={formData.responseContentPath || ""}
              onChange={(value) =>
                setFormData((prev) => ({
                  ...prev,
                  responseContentPath: value,
                }))
              }
              error={errors.responseContentPath}
              notes={form.pathNotes}
            />
          </div>

          <div className="flex justify-end gap-2 -mt-3">
            <Button
              variant="outline"
              onClick={() => setShowForm(!showForm)}
              className="h-11 border-1 border-input/50 focus:border-primary/50 transition-colors"
            >
              Cancel
            </Button>
            <Button
              onClick={handleSave}
              disabled={!formData.curl.trim()}
              className={cn(
                "h-11 border-1 border-input/50 focus:border-primary/50 transition-colors",
                errors.curl && "bg-red-500 hover:bg-red-600 text-white"
              )}
            >
              {errors.curl ? (
                "Invalid cURL, try again"
              ) : (
                <>
                  <SaveIcon className="h-4 w-4 mr-2" />
                  {editingProvider ? "Update" : "Save"} Provider
                </>
              )}
            </Button>
          </div>
        </Card>
      )}
    </>
  );
};
