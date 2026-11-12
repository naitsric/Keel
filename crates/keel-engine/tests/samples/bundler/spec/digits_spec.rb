require "rspec"

RSpec.describe "a dependency setup installed" do
  it "is the one the lock names" do
    expect(RSpec::Core::Version::STRING).to eq("3.13.7")
  end
end
