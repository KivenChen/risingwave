package com.risingwave.sql.tree;

import java.util.List;
import org.apache.commons.lang3.builder.EqualsBuilder;
import org.apache.commons.lang3.builder.HashCodeBuilder;
import org.apache.commons.lang3.builder.ToStringBuilder;

public final class CreateStream extends Statement {
  private final String name;
  private final List<Node> tableElements;
  private final GenericProperties<Expression> properties;
  private final String rowFormat;

  public CreateStream(
      String streamName,
      List<Node> tableElements,
      GenericProperties<Expression> props,
      String rowFormat) {
    this.name = streamName;
    this.tableElements = tableElements;
    this.properties = props;
    this.rowFormat = rowFormat;
  }

  public String getName() {
    return name;
  }

  public List<Node> getTableElements() {
    return tableElements;
  }

  public GenericProperties<Expression> getProperties() {
    return properties;
  }

  public String getRowFormat() {
    return rowFormat;
  }

  @Override
  public <R, C> R accept(AstVisitor<R, C> visitor, C context) {
    return visitor.visitCreateStream(this, context);
  }

  @Override
  public int hashCode() {
    return new HashCodeBuilder()
        .append(name)
        .append(tableElements)
        .append(properties)
        .append(rowFormat)
        .build();
  }

  @Override
  public boolean equals(Object o) {
    if (this == o) {
      return true;
    }
    if (o == null || getClass() != o.getClass()) {
      return false;
    }
    CreateStream rhs = (CreateStream) o;
    return new EqualsBuilder()
        .append(name, rhs.name)
        .append(tableElements, rhs.tableElements)
        .append(properties, rhs.properties)
        .append(rowFormat, rhs.rowFormat)
        .isEquals();
  }

  @Override
  public String toString() {
    return new ToStringBuilder(this)
        .append(name)
        .append(tableElements)
        .append(properties)
        .append(rowFormat)
        .build();
  }
}
