// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use jujutsu::testutils::TestEnvironment;

#[test]
fn test_no_commit_working_copy() {
    let test_env = TestEnvironment::default();
    test_env
        .jj_cmd(test_env.env_root(), &["init", "repo", "--git"])
        .assert()
        .success();

    let repo_path = test_env.env_root().join("repo");

    std::fs::write(repo_path.join("file"), "initial").unwrap();
    let expected_output = "@ 1e9ff0ea7220c37a1d2c4aab153e238c12ff3cd0
o 0000000000000000000000000000000000000000
";
    test_env
        .jj_cmd(&repo_path, &["log", "-T", "commit_id"])
        .assert()
        .success()
        .stdout(expected_output);

    // Modify the file. With --no-commit-working-copy, we still get the same commit
    // ID.
    std::fs::write(repo_path.join("file"), "modified").unwrap();
    test_env
        .jj_cmd(
            &repo_path,
            &["log", "-T", "commit_id", "--no-commit-working-copy"],
        )
        .assert()
        .success()
        .stdout(expected_output);

    // But without --no-commit-working-copy, we get a new commit ID.
    let expected_output = "@ cc12440b719c67fcd8c55848eb345f67b6e2d9f1
o 0000000000000000000000000000000000000000
";
    test_env
        .jj_cmd(&repo_path, &["log", "-T", "commit_id"])
        .assert()
        .success()
        .stdout(expected_output);
}

#[test]
fn test_repo_arg_with_init() {
    let test_env = TestEnvironment::default();
    test_env
        .jj_cmd(test_env.env_root(), &["init", "-R=.", "repo"])
        .assert()
        .failure()
        .stdout("Error: '--repository' cannot be used with 'init'\n");
}

#[test]
fn test_repo_arg_with_git_clone() {
    let test_env = TestEnvironment::default();
    test_env
        .jj_cmd(test_env.env_root(), &["git", "clone", "-R=.", "remote"])
        .assert()
        .failure()
        .stdout("Error: '--repository' cannot be used with 'git clone'\n");
}