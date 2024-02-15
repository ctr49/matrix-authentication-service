// Copyright 2022 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

import type { Meta, StoryObj } from "@storybook/react";

import { DummyRouter } from "../../test-utils/router";
import NavItem from "../NavItem";

import NavBar from "./NavBar";

const meta = {
  title: "UI/Nav Bar",
  component: NavBar,
  tags: ["autodocs"],
  render: (): React.ReactElement => (
    <DummyRouter>
      <NavBar>
        <NavItem to="/">Profile</NavItem>
        <NavItem to="/sessions">Sessions</NavItem>
      </NavBar>
    </DummyRouter>
  ),
} satisfies Meta<typeof NavBar>;

export default meta;
type Story = StoryObj<typeof NavBar>;

export const Basic: Story = {};
