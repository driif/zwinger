import { Card, CardBody, CardHeader } from "@driif/motiv";

const FEATURES = [
  {
    title: "Projects and cages",
    text: (
      <>
        A project is a folder on your Mac. Its cage is a Linux microVM with its own kernel. The agent works in the
        project folder as <code>/workspace</code>.
      </>
    ),
  },
  {
    title: "Your logins stay on the Mac",
    text: "The proxy of the cage adds your subscription login to the requests to the provider. The agent sees a placeholder only.",
  },
  {
    title: "Network control",
    text: "Block or allow each host or service per project. The app shows the hosts that the agent contacts and the hosts that the cage blocks.",
  },
  {
    title: "Tasks for two agents",
    text: "Two agents plan, build and review a task together. You see each stage in the run room.",
  },
];

export default function Features() {
  return (
    <div className="features not-content">
      {FEATURES.map((feature) => (
        <Card key={feature.title} variant="outlined">
          <CardHeader title={feature.title} titleAs="h2" />
          <CardBody>{feature.text}</CardBody>
        </Card>
      ))}
    </div>
  );
}
