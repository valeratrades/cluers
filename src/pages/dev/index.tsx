import { ProviderSection } from "./components";
import Contribute from "@/components/Contribute";
import { useSettings } from "@/hooks";
import { PageLayout } from "@/layouts";

const DevSpace = () => {
  const settings = useSettings();

  return (
    <PageLayout title="Dev Space" description="Manage your dev space">
      <Contribute />
      <ProviderSection kind="ai" {...settings} />
      <ProviderSection kind="stt" {...settings} />
    </PageLayout>
  );
};

export default DevSpace;
