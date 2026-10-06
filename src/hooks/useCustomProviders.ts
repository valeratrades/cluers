import { useState } from "react";
import { ProviderKind, TYPE_PROVIDER } from "@/types";
import { AI_PROVIDERS, SPEECH_TO_TEXT_PROVIDERS } from "@/config";
import { useApp } from "@/contexts";
import {
  getCustomProviders,
  saveCustomProvider,
  removeCustomProvider,
  validateCurl,
  deleteAllProviderSecrets,
} from "@/lib";

const EMPTY_FORM: TYPE_PROVIDER = {
  id: "",
  streaming: false,
  responseContentPath: "",
  isCustom: true,
  curl: "",
};

export function useCustomProviders(kind: ProviderKind) {
  const { loadData } = useApp();
  const [showForm, setShowForm] = useState(false);
  const [editingProvider, setEditingProvider] = useState<string | null>(null);
  const [formData, setFormData] = useState<TYPE_PROVIDER>(EMPTY_FORM);
  const [errors, setErrors] = useState<{ [key: string]: string }>({});
  const [deleteConfirm, setDeleteConfirm] = useState<string | null>(null);

  const handleEdit = (providerId: string) => {
    const provider = getCustomProviders(kind).find((p) => p.id === providerId);
    if (!provider) throw new Error(`unknown custom ${kind} provider: ${providerId}`);
    setFormData({ ...provider });
    setEditingProvider(providerId);
    setShowForm(!showForm);
    setErrors({});
  };

  const handleAutoFill = (providerId: string) => {
    const presets = kind === "ai" ? AI_PROVIDERS : SPEECH_TO_TEXT_PROVIDERS;
    const provider = presets.find((p) => p.id === providerId);
    if (!provider) return;
    setFormData({ ...provider, isCustom: true });
    setErrors({});
  };

  const handleDelete = (providerId: string) => {
    setDeleteConfirm(providerId);
  };

  const confirmDelete = async () => {
    if (!deleteConfirm) return;
    // secrets first: a re-added provider with the same id must not inherit them
    await deleteAllProviderSecrets(kind, deleteConfirm);
    removeCustomProvider(kind, deleteConfirm);
    setDeleteConfirm(null);
    loadData();
  };

  const cancelDelete = () => {
    setDeleteConfirm(null);
  };

  const handleSave = () => {
    const newErrors: { [key: string]: string } = {};

    if (!formData.curl.trim()) {
      newErrors.curl = "Curl command is required";
    } else {
      const validation = validateCurl(
        formData.curl,
        kind === "ai" ? ["TEXT"] : ["AUDIO"]
      );
      if (!validation.isValid) {
        newErrors.curl = validation.message || "";
      }
    }

    if (!formData.responseContentPath?.trim()) {
      newErrors.responseContentPath = "Response content path is required";
    }

    setErrors(newErrors);
    if (Object.keys(newErrors).length > 0) return;

    saveCustomProvider(kind, {
      id: editingProvider ?? "",
      isCustom: true,
      curl: formData.curl,
      streaming: kind === "stt" ? false : formData.streaming,
      responseContentPath: formData.responseContentPath,
    });
    setEditingProvider(null);
    setShowForm(false);
    setFormData(EMPTY_FORM);
    loadData();
  };

  return {
    errors,
    setErrors,
    showForm,
    setShowForm,
    editingProvider,
    setEditingProvider,
    deleteConfirm,
    formData,
    setFormData,
    handleSave,
    handleAutoFill,
    handleEdit,
    handleDelete,
    confirmDelete,
    cancelDelete,
  };
}
