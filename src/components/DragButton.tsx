import { Button } from "@/components";
import { GripVerticalIcon } from "lucide-react";

export const DragButton = () => {
  return (
    <Button
      variant="ghost"
      size="icon"
      className={`-ml-[2px] w-fit`}
      data-tauri-drag-region={true}
    >
      <GripVerticalIcon className="h-4 w-4" />
    </Button>
  );
};
