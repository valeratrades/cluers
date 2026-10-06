/**
 * Derive a conversation title from the first user message.
 * Currently a trim; lives here next to other small text helpers.
 */
export function generateConversationTitle(userMessage: string): string {
  return userMessage.trim();
}

/**
 * Enumerate `{{UPPERCASE}}` placeholders in a curl template. Used by
 * the settings UI to render the variable input form. `includeAll=true`
 * returns the well-known reserved tokens too (TEXT/IMAGE/SYSTEM_PROMPT/…)
 * which the AI request path always supplies itself.
 */
export function extractVariables(
  curl: string,
  includeAll = false
): { key: string; value: string }[] {
  if (typeof curl !== "string") {
    return [];
  }

  const regex = /\{\{([A-Z_]+)\}\}/g;
  const matches = curl?.match(regex) || [];
  const variables = matches
    .map((match) => {
      if (typeof match === "string") {
        return match.slice(2, -2);
      }
      return "";
    })
    .filter((v) => v !== "");

  const uniqueVariables = [...new Set(variables)];

  const doNotInclude = includeAll
    ? []
    : ["SYSTEM_PROMPT", "TEXT", "IMAGE", "IMAGE_MIME", "AUDIO", "DOCUMENT"];

  const filteredVariables = uniqueVariables?.filter(
    (variable) => !doNotInclude?.includes(variable)
  );

  return filteredVariables.map((variable) => ({
    key: variable?.toLowerCase()?.replace(/_/g, "_") || "",
    value: variable,
  }));
}
