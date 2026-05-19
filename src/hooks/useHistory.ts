import { useState, useEffect, useCallback } from "react";
import {
  listConversationSummaries,
  deleteConversation,
  DOWNLOAD_SUCCESS_DISPLAY_MS,
  type ConversationSummary,
} from "@/lib";
import { ChatConversation } from "@/types/completion";

export type UseHistoryType = ReturnType<typeof useHistory>;

export interface UseHistoryReturn {
  conversations: ConversationSummary[];
  selectedConversationId: string | null;
  deleteConfirm: string | null;
  isDownloaded: boolean;
  isAttached: boolean;

  handleDeleteConfirm: (conversationId: string) => void;
  confirmDelete: () => Promise<void>;
  cancelDelete: () => void;
  handleAttachToOverlay: (conversationId: string) => void;
  handleDownload: (
    conversation: ChatConversation | null,
    e: React.MouseEvent
  ) => void;
  search: string;
  setSearch: React.Dispatch<React.SetStateAction<string>>;
  refreshConversations: () => void;
  isLoading: boolean;
}

export function useHistory(): UseHistoryReturn {
  const [isLoading, setIsLoading] = useState(false);
  const [conversations, setConversations] = useState<ConversationSummary[]>([]);
  const [search, setSearch] = useState("");
  const [selectedConversationId] = useState<string | null>(null);

  const [deleteConfirm, setDeleteConfirm] = useState<string | null>(null);
  const [isDownloaded, setIsDownloaded] = useState(false);
  const [isAttached, setIsAttached] = useState(false);

  const refreshConversations = useCallback(async () => {
    try {
      setIsLoading(true);
      const summaries = await listConversationSummaries();
      setConversations(summaries);
    } catch (error) {
      console.error("Failed to load conversations:", error);
      setConversations([]);
    } finally {
      setIsLoading(false);
    }
  }, []);

  useEffect(() => {
    refreshConversations();
  }, [refreshConversations]);

  const handleDeleteConfirm = (conversationId: string) => {
    setDeleteConfirm(conversationId);
  };

  const confirmDelete = async () => {
    if (!deleteConfirm) return;

    try {
      await deleteConversation(deleteConfirm);
      setConversations((prev) => prev.filter((c) => c.id !== deleteConfirm));

      window.dispatchEvent(
        new CustomEvent("conversationDeleted", {
          detail: deleteConfirm,
        })
      );
    } catch (error) {
      console.error("Failed to delete conversation:", error);
    } finally {
      setDeleteConfirm(null);
    }
  };

  const cancelDelete = () => {
    setDeleteConfirm(null);
  };

  const handleAttachToOverlay = (conversationId: string) => {
    // Use localStorage to communicate between windows
    localStorage.setItem(
      "pluely-conversation-selected",
      JSON.stringify({ id: conversationId, timestamp: Date.now() })
    );
    setIsAttached(true);
    setTimeout(() => {
      setIsAttached(false);
    }, DOWNLOAD_SUCCESS_DISPLAY_MS);
  };

  const handleDownload = (
    conversation: ChatConversation | null,
    e: React.MouseEvent
  ) => {
    e.stopPropagation();
    if (!conversation) return;

    try {
      const markdown = generateConversationMarkdown(conversation);
      const blob = new Blob([markdown], { type: "text/markdown" });
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = generateFilename(conversation.title);
      document.body.appendChild(link);
      link.click();
      document.body.removeChild(link);
      URL.revokeObjectURL(url);
    } catch (error) {
      console.error("Failed to download conversation:", error);
      return;
    }

    setIsDownloaded(true);
    setTimeout(() => {
      setIsDownloaded(false);
    }, DOWNLOAD_SUCCESS_DISPLAY_MS);
  };

  const generateConversationMarkdown = (
    conversation: ChatConversation
  ): string => {
    let markdown = `# ${conversation.title}\n\n`;
    markdown += `**Created:** ${new Date(
      conversation.createdAt
    ).toLocaleString()}\n`;
    markdown += `**Updated:** ${new Date(
      conversation.updatedAt
    ).toLocaleString()}\n`;
    markdown += `**Messages:** ${conversation.messages.length}\n\n---\n\n`;

    conversation.messages.forEach((message, index) => {
      const roleLabel = message.role.toUpperCase();
      markdown += `## ${roleLabel}: ${message.content}\n`;

      if (index < conversation.messages.length - 1) {
        markdown += "\n";
      }
    });

    return markdown;
  };

  const generateFilename = (title: string): string => {
    const sanitizedTitle = title.replace(/[^a-z0-9]/gi, "_").toLowerCase();
    return `${sanitizedTitle.substring(0, 16)}.md`;
  };

  return {
    conversations,
    selectedConversationId,
    deleteConfirm,
    isDownloaded,
    isAttached,

    handleDeleteConfirm,
    confirmDelete,
    cancelDelete,
    handleAttachToOverlay,
    handleDownload,

    refreshConversations,
    search,
    setSearch,
    isLoading,
  };
}
