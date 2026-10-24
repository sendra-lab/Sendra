# JSON Schema / editor support

Sendra provides JSON Schema files for request files, collections, configuration
and environments. An editor can use them to offer autocomplete and flag
structural issues while you write YAML.

## Get the schemas

If Sendra is installed, run this command from your project directory:

```sh
sendra schema
```

It writes the schema files into a `schema/` directory. To choose another
location, pass `--output`:

```sh
sendra schema --output .vscode/sendra-schemas
```

The files cover these Sendra documents:

| Schema file | Covers |
| --- | --- |
| `request.schema.json` | A single request |
| `collection.schema.json` | A collection with a top-level `requests:` list |
| `config.schema.json` | `.sendra/config.yaml` |
| `environment.schema.json` | Files in `.sendra/environments/` |

## Configure VS Code

Install the [YAML extension](https://marketplace.visualstudio.com/items?itemName=redhat.vscode-yaml),
then add schema associations to `.vscode/settings.json`. Adjust the file
patterns to match the names and locations used in your project.

```json
{
  "yaml.schemas": {
    "./schema/request.schema.json": ["requests/*.yaml"],
    "./schema/collection.schema.json": ["collections/*.yaml"],
    "./schema/config.schema.json": [".sendra/config.yaml"],
    "./schema/environment.schema.json": [".sendra/environments/*.yaml"]
  }
}
```

If your request and collection files use overlapping patterns, make the
collection pattern more specific so the YAML extension can choose the right
schema. Sendra does not require a naming convention for request files.

## Schema validation limits

The schemas check document structure, including field names, value types and
required fields. Some rules can also be expressed structurally, such as
requiring a non-negative redirect count. Other rules depend on relationships
between fields and are checked when Sendra loads a file. For example, Sendra
requires a request to choose only one body format, and a collection's request
names must be unique.

An editor can therefore report a structurally valid file that Sendra later
rejects for a rule involving multiple fields. Run the request or test command
to validate the file with Sendra itself.
